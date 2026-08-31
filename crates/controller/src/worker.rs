//! Agent registry and the controller's work queue.
//!
//! The controller decides what to probe and what cost to apply; each known
//! agent owns a per-link work queue that the scheduler fills and the agent
//! drains by polling `POST /v1/agent/work`.

use crate::model::LinkKey;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Probe tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Always,
    Throughput,
}

/// A command the controller dispatches to an agent.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkCmd {
    Probe {
        id: String,
        tier: Tier,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        link: LinkKey,
        /// W3C `traceparent` of the dispatch span, echoed back by the agent so
        /// the reply/report spans join the same trace.
        #[serde(skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
    ApplyCost {
        id: String,
        link: LinkKey,
        cost: u32,
        /// W3C `traceparent` of the dispatch span (see above).
        #[serde(skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
}

/// Per-agent state: its managed links, pending work, last-issued timestamps
/// per tier, and the last cost the controller told it to apply.
#[derive(Default)]
struct Worker {
    links: Vec<LinkKey>,
    queue: VecDeque<WorkCmd>,
    /// link id -> cost the controller has sent (even if not yet acked)
    applied_sent: HashMap<String, u32>,
    /// link id -> unix secs of last issue per tier
    last_issue: HashMap<(String, Tier), u64>,
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
    /// The scheduler signals it after queuing work.
    pub fn notify(&self, agent: &str) -> Arc<Notify> {
        let mut map = self.notify_map.lock().expect("workers lock poisoned");
        map.entry(agent.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// Queue a command for an agent and wake its long-poll waiter. A probe is
    /// not enqueued if one of the same (link, tier) is already pending — the
    /// agent only ever has one outstanding probe per kind per link, so an
    /// unresponsive agent cannot accumulate an unbounded backlog. Returns true
    /// when the command was actually enqueued (so the caller stamps "issued").
    pub fn push(&self, agent: &str, cmd: WorkCmd) -> bool {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            let key = cmd_key(&cmd);
            if w.queue.iter().any(|c| cmd_key(c) == key) {
                drop(map);
                self.notify(agent).notify_waiters();
                return false;
            }
            w.queue.push_back(cmd);
            drop(map);
            self.notify(agent).notify_waiters();
            return true;
        }
        false
    }

    /// Register (or refresh) an agent and its managed links. Returns true when
    /// newly added (so the scheduler seeds the policy snapshot).
    ///
    /// A registration starts a clean slate: any work the scheduler queued while
    /// the agent was away is dropped, so a (re)connecting agent only receives
    /// commands issued after it came back.
    pub fn register(&self, agent: &str, links: Vec<LinkKey>) -> bool {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        let w = map.entry(agent.to_string()).or_default();
        w.last_seen = now_secs();
        w.queue.clear();
        if links.is_empty() {
            return false;
        }
        let mut fresh = false;
        for l in links {
            if !w.links.contains(&l) {
                // A new link gets no last-issue stamp, so it is due immediately.
                fresh = true;
                w.links.push(l);
            }
        }
        fresh
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

    /// Whether a link's always-on probe is due for this agent.
    pub fn always_due(&self, agent: &str, link: &LinkKey, now: u64, interval: u64) -> bool {
        let map = self.inner.lock().expect("workers lock poisoned");
        map.get(agent)
            .map(|w| w.is_due(&link.id(), Tier::Always, now, interval))
            .unwrap_or(false)
    }

    /// Whether a link's throughput probe is due for this agent.
    pub fn throughput_due(&self, agent: &str, link: &LinkKey, now: u64, interval: u64) -> bool {
        let map = self.inner.lock().expect("workers lock poisoned");
        map.get(agent)
            .map(|w| w.is_due(&link.id(), Tier::Throughput, now, interval))
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

    /// Remember the cost sent to an agent for a link.
    pub fn set_applied_sent(&self, agent: &str, link: &LinkKey, cost: u32) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            w.applied_sent.insert(link.id(), cost);
        }
    }

    /// Drain all pending commands for an agent (used by the work endpoint).
    pub fn drain(&self, agent: &str) -> Vec<WorkCmd> {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        match map.get_mut(agent) {
            Some(w) => {
                w.last_seen = now_secs();
                w.queue.drain(..).collect()
            }
            None => Vec::new(),
        }
    }

    pub fn touch(&self, agent: &str) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            w.last_seen = now_secs();
        }
    }

    pub fn agents(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("workers lock poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Dedup key for a queued command: link id + kind/tier.
fn cmd_key(c: &WorkCmd) -> (String, String) {
    match c {
        WorkCmd::Probe { link, tier, .. } => (link.id(), format!("probe/{tier:?}")),
        WorkCmd::ApplyCost { link, .. } => (link.id(), "apply".into()),
    }
}

/// Request bodies for the agent endpoints.
#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub agent: String,
    #[serde(default)]
    pub links: Vec<LinkKey>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResp {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<WorkCmd>,
}

#[derive(Debug, Deserialize)]
pub struct WorkReq {
    pub agent: String,
}

#[derive(Debug, Serialize)]
pub struct WorkResp {
    pub commands: Vec<WorkCmd>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyAckReq {
    pub agent: String,
    pub link: LinkKey,
    pub cost: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LinkKey;

    fn link() -> LinkKey {
        LinkKey::new("spoke-1", "hub-a", "awg_hub_a")
    }

    #[test]
    fn register_adds_links_and_marks_owned() {
        let r = WorkerRegistry::new();
        assert!(r.register("spoke-1", vec![link()]));
        assert_eq!(r.len(), 1);
        let owned = r.owned_links();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].0, "spoke-1");
    }

    #[test]
    fn always_due_at_first_then_not_until_interval() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()]);
        assert!(r.always_due("spoke-1", &link(), 1000, 15));
        r.mark_issued("spoke-1", &link(), Tier::Always, 1000);
        assert!(!r.always_due("spoke-1", &link(), 1010, 15));
        assert!(r.always_due("spoke-1", &link(), 1015, 15));
    }

    #[test]
    fn pending_commands_are_drained_once() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()]);
        r.push(
            "spoke-1",
            WorkCmd::Probe {
                id: "x".into(),
                tier: Tier::Always,
                token: None,
                link: link(),
                traceparent: None,
            },
        );
        assert_eq!(r.drain("spoke-1").len(), 1);
        assert_eq!(r.drain("spoke-1").len(), 0);
    }

    #[test]
    fn applied_sent_tracking() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()]);
        assert_eq!(r.applied_sent("spoke-1", &link()), None);
        r.set_applied_sent("spoke-1", &link(), 50);
        assert_eq!(r.applied_sent("spoke-1", &link()), Some(50));
    }

    #[test]
    fn push_dedups_same_link_tier() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()]);
        let probe = |n: &str| WorkCmd::Probe {
            id: n.into(),
            tier: Tier::Always,
            token: None,
            link: link(),
            traceparent: None,
        };
        // Same (link, tier) queued repeatedly → only one makes it in.
        assert!(r.push("spoke-1", probe("a")));
        assert!(!r.push("spoke-1", probe("b")));
        assert_eq!(r.drain("spoke-1").len(), 1);
    }

    #[test]
    fn register_drops_stale_queued_work() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()]);
        r.push(
            "spoke-1",
            WorkCmd::Probe {
                id: "stale".into(),
                tier: Tier::Throughput,
                token: Some("tok".into()),
                link: link(),
                traceparent: None,
            },
        );
        // Re-register (e.g. agent reconnect) clears anything queued while away.
        assert!(!r.register("spoke-1", vec![link()]));
        assert!(r.drain("spoke-1").is_empty());
        // New work is accepted after the reset.
        r.push(
            "spoke-1",
            WorkCmd::Probe {
                id: "fresh".into(),
                tier: Tier::Always,
                token: None,
                link: link(),
                traceparent: None,
            },
        );
        assert_eq!(r.drain("spoke-1").len(), 1);
    }
}
