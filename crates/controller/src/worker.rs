//! Agent registry and the controller's work queue.
//!
//! The controller decides what to probe and what cost to apply; each known
//! agent owns a per-link work queue that the scheduler fills and the agent
//! drains through the `POST /v1/agent/command` long-poll.

use crate::clock::now_secs;
use crate::model::LinkKey;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub use mielofon_proto::{RegisterReq, RegisterResp, Tier, WorkCmd, WorkResp};

/// How long (seconds) a failed `apply_cost` is left alone before the
/// scheduler re-sends it, so a BIRD that keeps rejecting a cost is not
/// hammered every tick.
pub const APPLY_RETRY_SECS: u64 = 30;

/// Per-agent state: its managed links, pending work, last-issued timestamps
/// per tier, the last cost the controller told it to apply, and the mesh
/// loopback it reported at register (the trace walker's "to" resolution).
#[derive(Default)]
struct Worker {
    links: Vec<LinkKey>,
    queue: VecDeque<WorkCmd>,
    loopback: Option<String>,
    /// link id -> cost the controller has sent (even if not yet acked)
    applied_sent: HashMap<String, u32>,
    /// link id -> unix secs before which a failed apply is not re-sent
    apply_retry_at: HashMap<String, u64>,
    /// link id -> unix secs of last issue per tier
    last_issue: HashMap<(String, Tier), u64>,
    /// Unix secs of the last register or command poll.
    last_seen: u64,
}

impl Worker {
    fn is_due(&self, link: &str, tier: Tier, now: u64, interval: u64) -> bool {
        match self.last_issue.get(&(link.to_string(), tier)) {
            Some(t) => now.saturating_sub(*t) >= interval,
            None => true,
        }
    }

    fn mark_issued(&mut self, link: &str, tier: Tier, now: u64) {
        self.last_issue.insert((link.to_string(), tier), now);
    }
}

/// Thread-safe registry of agents.
#[derive(Default)]
pub struct WorkerRegistry {
    inner: Mutex<HashMap<String, Worker>>,
    notify_map: Mutex<HashMap<String, Arc<Notify>>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The per-agent wake-up handle used by `/v1/agent/command` long-poll.
    /// The scheduler signals it after queuing work. `notify_one` stores a
    /// permit when nobody is waiting, so a command queued between the poll's
    /// drain and its wait is never missed.
    pub fn notify(&self, agent: &str) -> Arc<Notify> {
        let mut map = self.notify_map.lock().expect("workers lock poisoned");
        map.entry(agent.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    fn enqueue(&self, agent: &str, cmd: WorkCmd, front: bool) -> bool {
        let queued = {
            let mut map = self.inner.lock().expect("workers lock poisoned");
            match map.get_mut(agent) {
                Some(w) => {
                    let key = cmd_key(&cmd);
                    if w.queue.iter().any(|c| cmd_key(c) == key) {
                        false
                    } else {
                        if front {
                            w.queue.push_front(cmd);
                        } else {
                            w.queue.push_back(cmd);
                        }
                        true
                    }
                }
                None => return false,
            }
        };
        self.notify(agent).notify_one();
        queued
    }

    /// Queue a command for an agent and wake its long-poll waiter. A probe is
    /// not enqueued if one of the same (link, tier) is already pending — the
    /// agent only ever has one outstanding probe per kind per link, so an
    /// unresponsive agent cannot accumulate an unbounded backlog. Returns true
    /// when the command was actually enqueued (so the caller stamps "issued").
    pub fn push(&self, agent: &str, cmd: WorkCmd) -> bool {
        self.enqueue(agent, cmd, false)
    }

    /// Queue a command at the FRONT of an agent's queue. Trace hops jump the
    /// probe backlog (a trace must answer promptly, not wait behind a
    /// throughput test), so the walker's per-hop timeout does not expire
    /// because the command sat queued. Trace commands are deduped by id only,
    /// so a retried hop is never silently dropped.
    pub fn push_front(&self, agent: &str, cmd: WorkCmd) -> bool {
        self.enqueue(agent, cmd, true)
    }

    /// Register (or refresh) an agent with its current link set. Returns true
    /// when a link is new to the registry (it is due for probing right away).
    ///
    /// A registration is a clean slate: queued work is dropped, links the
    /// agent no longer reports are forgotten, and the "cost sent" memory is
    /// cleared so the policy snapshot is re-seeded — a re-registering agent
    /// may have rebooted and lost its applied costs.
    pub fn register(&self, agent: &str, links: Vec<LinkKey>, loopback: Option<String>) -> bool {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        let w = map.entry(agent.to_string()).or_default();
        w.last_seen = now_secs();
        w.queue.clear();
        w.loopback = loopback;
        w.applied_sent.clear();
        w.apply_retry_at.clear();
        let fresh = links.iter().any(|l| !w.links.contains(l));
        let keep: HashSet<String> = links.iter().map(LinkKey::id).collect();
        w.last_issue.retain(|(id, _), _| keep.contains(id));
        w.links = links;
        fresh
    }

    /// Forget agents that have neither registered nor polled for `max_idle`
    /// seconds. Returns the evicted names.
    pub fn evict_idle(&self, now: u64, max_idle: u64) -> Vec<String> {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        let gone: Vec<String> = map
            .iter()
            .filter(|(_, w)| now.saturating_sub(w.last_seen) > max_idle)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &gone {
            map.remove(name);
        }
        drop(map);
        if !gone.is_empty() {
            let mut notify = self.notify_map.lock().expect("workers lock poisoned");
            for name in &gone {
                notify.remove(name);
            }
        }
        gone
    }

    /// How many known agents there are.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("workers lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All (agent, link) pairs owned by registered agents.
    pub fn owned_links(&self) -> Vec<(String, LinkKey)> {
        let map = self.inner.lock().expect("workers lock poisoned");
        let mut out = Vec::new();
        for (name, w) in map.iter() {
            for l in &w.links {
                out.push((name.clone(), l.clone()));
            }
        }
        out
    }

    /// Links of one registered agent.
    pub fn links_of(&self, agent: &str) -> Vec<LinkKey> {
        let map = self.inner.lock().expect("workers lock poisoned");
        map.get(agent).map(|w| w.links.clone()).unwrap_or_default()
    }

    /// Whether a link's probe of `tier` is due for this agent.
    pub fn probe_due(
        &self,
        agent: &str,
        link: &LinkKey,
        tier: Tier,
        now: u64,
        interval: u64,
    ) -> bool {
        let map = self.inner.lock().expect("workers lock poisoned");
        map.get(agent)
            .map(|w| w.is_due(&link.id(), tier, now, interval))
            .unwrap_or(false)
    }

    /// Record that a probe of a tier was issued right now.
    pub fn mark_issued(&self, agent: &str, link: &LinkKey, tier: Tier, now: u64) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            w.mark_issued(&link.id(), tier, now);
        }
    }

    /// The last cost the controller asked the agent to apply for a link, if any.
    pub fn applied_sent(&self, agent: &str, link: &LinkKey) -> Option<u32> {
        let map = self.inner.lock().expect("workers lock poisoned");
        map.get(agent)
            .and_then(|w| w.applied_sent.get(&link.id()))
            .copied()
    }

    /// Whether `cost` should be (re-)sent: it differs from the last cost sent
    /// and no failed apply is still in its retry back-off.
    pub fn apply_due(&self, agent: &str, link: &LinkKey, cost: u32, now: u64) -> bool {
        let map = self.inner.lock().expect("workers lock poisoned");
        let Some(w) = map.get(agent) else {
            return false;
        };
        let id = link.id();
        w.applied_sent.get(&id) != Some(&cost)
            && w.apply_retry_at.get(&id).is_none_or(|t| now >= *t)
    }

    /// Remember the cost sent to an agent for a link.
    pub fn set_applied_sent(&self, agent: &str, link: &LinkKey, cost: u32) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            let id = link.id();
            w.apply_retry_at.remove(&id);
            w.applied_sent.insert(id, cost);
        }
    }

    /// The agent could not apply the cost: forget it was sent so the scheduler
    /// re-issues it after `APPLY_RETRY_SECS`.
    pub fn note_apply_failed(&self, agent: &str, link: &LinkKey, now: u64) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            let id = link.id();
            w.applied_sent.remove(&id);
            w.apply_retry_at.insert(id, now + APPLY_RETRY_SECS);
        }
    }

    /// Drain all pending commands for an agent; `None` when the agent is not
    /// registered (it must register before it can poll).
    pub fn drain(&self, agent: &str) -> Option<Vec<WorkCmd>> {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        let w = map.get_mut(agent)?;
        w.last_seen = now_secs();
        Some(w.queue.drain(..).collect())
    }

    pub fn agents(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("workers lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Whether an agent is currently registered.
    pub fn has_agent(&self, agent: &str) -> bool {
        self.inner
            .lock()
            .expect("workers lock poisoned")
            .contains_key(agent)
    }

    /// The mesh loopback an agent reported at register, if any. This is how
    /// the trace endpoint resolves a destination *node name* to the prefix the
    /// walker asks each hop to resolve (see `crates/controller/src/trace.rs`).
    pub fn loopback(&self, agent: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("workers lock poisoned")
            .get(agent)
            .and_then(|w| w.loopback.clone())
    }
}

/// Dedup key for a queued command: link id + kind/tier.
fn cmd_key(c: &WorkCmd) -> (String, String) {
    match c {
        WorkCmd::Probe { link, tier, .. } => (link.id(), format!("probe/{tier:?}")),
        WorkCmd::ApplyCost { link, .. } => (link.id(), "apply".into()),
        // Trace commands are unique per dispatch — the cmd id is the dedup key,
        // so a walker that retries a dead/unresponsive hop cannot silently lose
        // a command behind an identical one that is already queued.
        WorkCmd::Trace { id, .. } => (id.clone(), "trace".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LinkKey;

    fn link() -> LinkKey {
        LinkKey::new("spoke-1", "hub-a", "awg_hub_a")
    }

    fn probe(id: &str, tier: Tier) -> WorkCmd {
        WorkCmd::Probe {
            id: id.into(),
            tier,
            token: None,
            link: link(),
            traceparent: None,
        }
    }

    #[test]
    fn register_adds_links_and_marks_owned() {
        let r = WorkerRegistry::new();
        assert!(r.register("spoke-1", vec![link()], None));
        assert_eq!(r.len(), 1);
        let owned = r.owned_links();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].0, "spoke-1");
    }

    #[test]
    fn register_replaces_the_link_set() {
        let r = WorkerRegistry::new();
        let gone = LinkKey::new("spoke-1", "hub-b", "awg_hub_b");
        r.register("spoke-1", vec![link(), gone.clone()], None);
        r.mark_issued("spoke-1", &gone, Tier::Always, 1000);
        // The tunnel to hub-b disappeared: it must not be probed any more.
        assert!(!r.register("spoke-1", vec![link()], None));
        assert_eq!(r.links_of("spoke-1"), vec![link()]);
        // It comes back later: due immediately again (stamp was dropped).
        assert!(r.register("spoke-1", vec![link(), gone.clone()], None));
        assert!(r.probe_due("spoke-1", &gone, Tier::Always, 1001, 15));
    }

    #[test]
    fn register_forgets_sent_costs() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        r.set_applied_sent("spoke-1", &link(), 50);
        assert!(!r.apply_due("spoke-1", &link(), 50, 0));
        // A rebooted node lost its applied costs: re-seed on re-register.
        r.register("spoke-1", vec![link()], None);
        assert_eq!(r.applied_sent("spoke-1", &link()), None);
        assert!(r.apply_due("spoke-1", &link(), 50, 0));
    }

    #[test]
    fn always_due_at_first_then_not_until_interval() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        assert!(r.probe_due("spoke-1", &link(), Tier::Always, 1000, 15));
        r.mark_issued("spoke-1", &link(), Tier::Always, 1000);
        assert!(!r.probe_due("spoke-1", &link(), Tier::Always, 1010, 15));
        assert!(r.probe_due("spoke-1", &link(), Tier::Always, 1015, 15));
    }

    #[test]
    fn pending_commands_are_drained_once() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        r.push("spoke-1", probe("x", Tier::Always));
        assert_eq!(r.drain("spoke-1").unwrap().len(), 1);
        assert_eq!(r.drain("spoke-1").unwrap().len(), 0);
    }

    #[test]
    fn unregistered_agent_cannot_drain() {
        let r = WorkerRegistry::new();
        assert!(r.drain("spoke-1").is_none());
        assert!(!r.push("spoke-1", probe("x", Tier::Always)));
    }

    #[test]
    fn failed_apply_is_retried_after_backoff() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        r.set_applied_sent("spoke-1", &link(), 50);
        r.note_apply_failed("spoke-1", &link(), 1000);
        assert!(!r.apply_due("spoke-1", &link(), 50, 1000 + APPLY_RETRY_SECS - 1));
        assert!(r.apply_due("spoke-1", &link(), 50, 1000 + APPLY_RETRY_SECS));
        // A successful ack clears the back-off.
        r.set_applied_sent("spoke-1", &link(), 50);
        assert!(!r.apply_due("spoke-1", &link(), 50, 5000));
        assert!(r.apply_due("spoke-1", &link(), 20, 5000));
    }

    #[test]
    fn push_dedups_same_link_tier() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        // Same (link, tier) queued repeatedly → only one makes it in.
        assert!(r.push("spoke-1", probe("a", Tier::Always)));
        assert!(!r.push("spoke-1", probe("b", Tier::Always)));
        assert_eq!(r.drain("spoke-1").unwrap().len(), 1);
    }

    #[test]
    fn register_drops_stale_queued_work() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        r.push("spoke-1", probe("stale", Tier::Throughput));
        // Re-register (e.g. agent reconnect) clears anything queued while away.
        assert!(!r.register("spoke-1", vec![link()], None));
        assert!(r.drain("spoke-1").unwrap().is_empty());
        // New work is accepted after the reset.
        r.push("spoke-1", probe("fresh", Tier::Always));
        assert_eq!(r.drain("spoke-1").unwrap().len(), 1);
    }

    #[test]
    fn idle_agents_are_evicted() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None);
        let now = now_secs();
        assert!(r.evict_idle(now + 100, 300).is_empty());
        assert_eq!(r.evict_idle(now + 301, 300), vec!["spoke-1".to_string()]);
        assert!(!r.has_agent("spoke-1"));
    }

    #[test]
    fn loopback_register_lookup() {
        let r = WorkerRegistry::new();
        assert_eq!(r.loopback("spoke-1"), None);
        assert!(!r.has_agent("spoke-1"));
        r.register("spoke-1", vec![link()], Some("fd00:0:0:1::1".into()));
        assert_eq!(r.loopback("spoke-1"), Some("fd00:0:0:1::1".into()));
        assert!(r.has_agent("spoke-1"));
        // Re-register without a loopback drops it (agent cannot report it).
        r.register("spoke-1", vec![link()], None);
        assert_eq!(r.loopback("spoke-1"), None);
    }
}
