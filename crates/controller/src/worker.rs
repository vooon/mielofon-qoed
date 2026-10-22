//! Agent registry: which agents are connected, which links they own, the
//! session each command is dispatched on, and the per-link bookkeeping the
//! scheduler needs (last issue per tier, cost sent, commands in flight).
//!
//! There is no work queue: a command is a stream the controller opens on
//! the agent's session (`session::dispatch`). "In flight" replaces queue
//! dedup — at most one command per (link, kind) is outstanding, so a slow
//! agent never accumulates a backlog.

use crate::model::LinkKey;
use mielofon_transport::Session;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use mielofon_proto::{RegisterReq, RegisterResp, Tier, WorkCmd};

/// How long (seconds) a failed `apply_cost` is left alone before the
/// scheduler re-sends it, so a BIRD that keeps rejecting a cost is not
/// hammered every tick.
pub const APPLY_RETRY_SECS: u64 = 30;

/// What is tracked as in flight per link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Always,
    Throughput,
    Apply,
}

/// The (link, kind) a command occupies while outstanding; traces are not
/// per link and run under the walker's own control.
pub fn track_key(cmd: &WorkCmd) -> Option<(LinkKey, Kind)> {
    match cmd {
        WorkCmd::Probe {
            link,
            tier: Tier::Always,
            ..
        } => Some((link.clone(), Kind::Always)),
        WorkCmd::Probe {
            link,
            tier: Tier::Throughput,
            ..
        } => Some((link.clone(), Kind::Throughput)),
        WorkCmd::ApplyCost { link, .. } => Some((link.clone(), Kind::Apply)),
        WorkCmd::Trace { .. } => None,
    }
}

/// Per-agent state.
#[derive(Default)]
struct Worker {
    links: Vec<LinkKey>,
    /// The node's own router id, the key of its directory entry.
    router_id: Option<String>,
    /// interface → router id of the neighbour, for naming `to` exactly.
    peer_rids: HashMap<String, String>,
    loopback: Option<String>,
    /// The live session and its generation (bumps on every new connection).
    session: Option<(u64, Arc<dyn Session>)>,
    /// link id -> cost the controller has sent (even if not yet acked)
    applied_sent: HashMap<String, u32>,
    /// link id -> unix secs before which a failed apply is not re-sent
    apply_retry_at: HashMap<String, u64>,
    /// link id -> unix secs of last issue per tier
    last_issue: HashMap<(String, Tier), u64>,
    /// Commands outstanding on the session.
    in_flight: HashSet<(String, Kind)>,
}

impl Worker {
    fn is_due(&self, link: &str, tier: Tier, now: u64, interval: u64) -> bool {
        match self.last_issue.get(&(link.to_string(), tier)) {
            Some(t) => now.saturating_sub(*t) >= interval,
            None => true,
        }
    }
}

/// Thread-safe registry of agents.
/// Matching key for node names: what a BIRD protocol name can carry
/// (`peer_node_0_example`) and the hostname it stands for (`node-0.example`) agree
/// on, once dots and hyphens are folded into underscores.
fn name_key(name: &str) -> String {
    name.to_ascii_lowercase().replace(['-', '.'], "_")
}

#[derive(Default)]
pub struct WorkerRegistry {
    inner: Mutex<HashMap<String, Worker>>,
    generation: AtomicU64,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or refresh) an agent with its current link set. Returns true
    /// when a link is new to the registry (it is due for probing right away).
    ///
    /// A registration is a clean slate for policy: links the agent no longer
    /// reports are forgotten and the "cost sent" memory is cleared so the
    /// policy snapshot is re-seeded — a re-registering agent may have
    /// rebooted and lost its applied costs. The session, if any, is kept.
    pub fn register(
        &self,
        agent: &str,
        mut links: Vec<LinkKey>,
        router_id: Option<String>,
        peer_rids: HashMap<String, String>,
        loopback: Option<String>,
    ) -> bool {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        // Peer names come from BIRD protocol names, which cannot spell a
        // hyphen: `peer_node_0_example` decodes to `node.0.example` while that node
        // registers as `node-0.example`. Settle every link on the name the
        // peer registers under, in both directions of time.
        let me = name_key(agent);
        for l in &mut links {
            if let Some(known) = map.keys().find(|k| name_key(k) == name_key(&l.to)) {
                l.to = known.clone();
            } else if name_key(&l.to) == me {
                l.to = agent.to_string();
            }
        }
        for (name, other) in map.iter_mut() {
            if name == agent {
                continue;
            }
            for l in &mut other.links {
                if l.to != agent && name_key(&l.to) == me {
                    l.to = agent.to_string();
                }
            }
        }
        let w = map.entry(agent.to_string()).or_default();
        w.router_id = router_id;
        w.peer_rids = peer_rids;
        w.loopback = loopback;
        w.applied_sent.clear();
        w.apply_retry_at.clear();
        let fresh = links.iter().any(|l| !w.links.contains(l));
        let keep: HashSet<String> = links.iter().map(LinkKey::id).collect();
        w.last_issue.retain(|(id, _), _| keep.contains(id));
        w.in_flight.retain(|(id, _)| keep.contains(id));
        w.links = links;
        fresh
    }

    /// Name every link's far end by router id through `lookup` (the
    /// replicated node directory): a hub's agent knows a spoke only by the
    /// id OSPF shows it, the directory knows what that node registered as.
    /// Returns the agents whose link set changed, with their router id,
    /// so their directory entries can be republished.
    pub fn resolve_peers(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Vec<(String, Option<String>, Vec<LinkKey>)> {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        let mut renamed = Vec::new();
        for (agent, w) in map.iter_mut() {
            let mut changed = false;
            for l in &mut w.links {
                let Some(name) = w.peer_rids.get(&l.interface).and_then(|rid| lookup(rid)) else {
                    continue;
                };
                if l.to != name {
                    l.to = name;
                    changed = true;
                }
            }
            if changed {
                renamed.push((agent.clone(), w.router_id.clone(), w.links.clone()));
            }
        }
        renamed
    }

    /// Every agent with its router id and current links, for the directory.
    pub fn entries(&self) -> Vec<(String, Option<String>, Vec<LinkKey>)> {
        self.inner
            .lock()
            .expect("workers lock poisoned")
            .iter()
            .map(|(a, w)| (a.clone(), w.router_id.clone(), w.links.clone()))
            .collect()
    }

    /// The router id `agent` registered with.
    pub fn router_id_of(&self, agent: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("workers lock poisoned")
            .get(agent)
            .and_then(|w| w.router_id.clone())
    }

    /// The registered spelling of a peer name an agent reported (see
    /// `register`): `node.0.example` → `node-0.example` once that node is
    /// known. Unknown names pass through.
    pub fn canonical_peer(&self, name: &str) -> String {
        let map = self.inner.lock().expect("workers lock poisoned");
        if map.contains_key(name) {
            return name.to_string();
        }
        let key = name_key(name);
        map.keys()
            .find(|k| name_key(k) == key)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// Attach the session commands for `agent` are dispatched on. `own` is
    /// the generation this connection already holds, if it registered
    /// before (a link-set refresh): then nothing changes. Otherwise the
    /// session replaces the previous one, which is returned so the caller
    /// can close it. Returns the generation the caller must detach with.
    pub fn attach(
        &self,
        agent: &str,
        session: Arc<dyn Session>,
        own: Option<u64>,
    ) -> (u64, Option<Arc<dyn Session>>) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        let w = map.entry(agent.to_string()).or_default();
        if let (Some(own), Some((cur, _))) = (own, &w.session) {
            if own == *cur {
                return (own, None);
            }
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let old = w.session.replace((generation, session)).map(|(_, s)| s);
        w.in_flight.clear();
        (generation, old)
    }

    /// The session of `agent` went away: forget the agent, unless a newer
    /// session has replaced it meanwhile. Returns true when evicted.
    pub fn detach(&self, agent: &str, generation: u64) -> bool {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        match map.get(agent) {
            Some(w) if w.session.as_ref().is_some_and(|(g, _)| *g == generation) => {
                map.remove(agent);
                true
            }
            _ => false,
        }
    }

    /// The live session of an agent, if it has one.
    pub fn session(&self, agent: &str) -> Option<Arc<dyn Session>> {
        let map = self.inner.lock().expect("workers lock poisoned");
        map.get(agent)
            .and_then(|w| w.session.as_ref().map(|(_, s)| s.clone()))
    }

    /// Claim the (link, kind) slot for a command about to be dispatched.
    /// False when the agent is unknown or a command of that kind is already
    /// outstanding on the link.
    pub fn begin(&self, agent: &str, link: &LinkKey, kind: Kind) -> bool {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        match map.get_mut(agent) {
            Some(w) => w.in_flight.insert((link.id(), kind)),
            None => false,
        }
    }

    /// Release the slot once the command answered or failed.
    pub fn end(&self, agent: &str, link: &LinkKey, kind: Kind) {
        let mut map = self.inner.lock().expect("workers lock poisoned");
        if let Some(w) = map.get_mut(agent) {
            w.in_flight.remove(&(link.id(), kind));
        }
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
            w.last_issue.insert((link.id(), tier), now);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LinkKey;
    use crate::session::testing::FakeAgent;

    fn link() -> LinkKey {
        LinkKey::new("spoke-1", "hub-a", "awg_hub_a")
    }

    #[test]
    fn register_adds_links_and_marks_owned() {
        let r = WorkerRegistry::new();
        assert!(r.register("spoke-1", vec![link()], None, HashMap::new(), None));
        assert_eq!(r.len(), 1);
        let owned = r.owned_links();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].0, "spoke-1");
    }

    #[test]
    fn peer_names_settle_on_the_registered_spelling() {
        // A BIRD protocol name cannot spell the hyphen in `node-0`, so the
        // hub decodes its peer as `node.0.example`. Whichever side comes
        // first, the link ends up on the name the spoke registers under.
        let r = WorkerRegistry::new();
        let hub_link = LinkKey::new("hub-a", "node.0.example", "awg_sp_node0");
        r.register("hub-a", vec![hub_link], None, HashMap::new(), None);
        r.register(
            "node-0.example",
            vec![LinkKey::new("node-0.example", "hub-a", "awg_hub_a")],
            None,
            HashMap::new(),
            None,
        );
        assert_eq!(r.links_of("hub-a")[0].to, "node-0.example", "rewritten");

        let r = WorkerRegistry::new();
        r.register(
            "node-0.example",
            vec![LinkKey::new("node-0.example", "hub-a", "awg_hub_a")],
            None,
            HashMap::new(),
            None,
        );
        r.register(
            "hub-a",
            vec![LinkKey::new("hub-a", "node.0.example", "awg_sp_node0")],
            None,
            HashMap::new(),
            None,
        );
        assert_eq!(
            r.links_of("hub-a")[0].to,
            "node-0.example",
            "canonical at register"
        );
        assert_eq!(r.links_of("node-0.example")[0].to, "hub-a", "untouched");
        assert_eq!(r.canonical_peer("node.0.example"), "node-0.example");
        assert_eq!(r.canonical_peer("hub-a"), "hub-a");
        assert_eq!(r.canonical_peer("nobody.example"), "nobody.example");
    }

    #[test]
    fn peers_are_named_by_router_id_through_the_directory() {
        let r = WorkerRegistry::new();
        let rids: HashMap<String, String> =
            [("awg_sp_n0".to_string(), "192.0.2.9".to_string())].into();
        r.register(
            "hub-a",
            vec![LinkKey::new("hub-a", "node.0.example", "awg_sp_n0")],
            Some("192.0.2.1".into()),
            rids,
            None,
        );
        assert_eq!(r.router_id_of("hub-a").as_deref(), Some("192.0.2.1"));
        let dir: HashMap<&str, &str> = [("192.0.2.9", "node-0.example")].into();
        let renamed = r.resolve_peers(|rid| dir.get(rid).map(|s| s.to_string()));
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].0, "hub-a");
        assert_eq!(renamed[0].1.as_deref(), Some("192.0.2.1"));
        assert_eq!(renamed[0].2[0].to, "node-0.example");
        assert_eq!(r.links_of("hub-a")[0].to, "node-0.example");
        assert!(
            r.resolve_peers(|rid| dir.get(rid).map(|s| s.to_string()))
                .is_empty(),
            "idempotent"
        );
        assert!(
            r.resolve_peers(|_| None).is_empty(),
            "unknown rid: name kept"
        );
    }

    #[test]
    fn register_replaces_the_link_set() {
        let r = WorkerRegistry::new();
        let gone = LinkKey::new("spoke-1", "hub-b", "awg_hub_b");
        r.register(
            "spoke-1",
            vec![link(), gone.clone()],
            None,
            HashMap::new(),
            None,
        );
        r.mark_issued("spoke-1", &gone, Tier::Always, 1000);
        // The tunnel to hub-b disappeared: it must not be probed any more.
        assert!(!r.register("spoke-1", vec![link()], None, HashMap::new(), None));
        assert_eq!(r.links_of("spoke-1"), vec![link()]);
        // It comes back later: due immediately again (stamp was dropped).
        assert!(r.register(
            "spoke-1",
            vec![link(), gone.clone()],
            None,
            HashMap::new(),
            None
        ));
        assert!(r.probe_due("spoke-1", &gone, Tier::Always, 1001, 15));
    }

    #[test]
    fn register_forgets_sent_costs() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
        r.set_applied_sent("spoke-1", &link(), 50);
        assert!(!r.apply_due("spoke-1", &link(), 50, 0));
        // A rebooted node lost its applied costs: re-seed on re-register.
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
        assert_eq!(r.applied_sent("spoke-1", &link()), None);
        assert!(r.apply_due("spoke-1", &link(), 50, 0));
    }

    #[test]
    fn always_due_at_first_then_not_until_interval() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
        assert!(r.probe_due("spoke-1", &link(), Tier::Always, 1000, 15));
        r.mark_issued("spoke-1", &link(), Tier::Always, 1000);
        assert!(!r.probe_due("spoke-1", &link(), Tier::Always, 1010, 15));
        assert!(r.probe_due("spoke-1", &link(), Tier::Always, 1015, 15));
    }

    #[test]
    fn failed_apply_is_retried_after_backoff() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
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
    fn one_command_per_link_and_kind_in_flight() {
        let r = WorkerRegistry::new();
        assert!(!r.begin("spoke-1", &link(), Kind::Always), "unknown agent");
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
        assert!(r.begin("spoke-1", &link(), Kind::Always));
        assert!(
            !r.begin("spoke-1", &link(), Kind::Always),
            "still outstanding"
        );
        assert!(r.begin("spoke-1", &link(), Kind::Throughput), "other kind");
        r.end("spoke-1", &link(), Kind::Always);
        assert!(r.begin("spoke-1", &link(), Kind::Always));
    }

    #[tokio::test]
    async fn sessions_supersede_and_detach_by_generation() {
        let r = WorkerRegistry::new();
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
        let first = FakeAgent::silent();
        let (g1, old) = r.attach("spoke-1", first.clone(), None);
        assert!(old.is_none());
        // The same connection refreshing its links keeps its generation.
        let (g1b, old) = r.attach("spoke-1", first.clone(), Some(g1));
        assert_eq!(g1b, g1);
        assert!(old.is_none());
        // A new connection for the same agent supersedes the old one.
        let second = FakeAgent::silent();
        let (g2, old) = r.attach("spoke-1", second, None);
        assert_ne!(g2, g1);
        assert!(old.is_some(), "the superseded session is handed back");
        // The old connection closing must not evict the new registration.
        assert!(!r.detach("spoke-1", g1));
        assert!(r.has_agent("spoke-1"));
        assert!(r.detach("spoke-1", g2));
        assert!(!r.has_agent("spoke-1"));
        assert!(r.session("spoke-1").is_none());
    }

    #[test]
    fn loopback_register_lookup() {
        let r = WorkerRegistry::new();
        assert_eq!(r.loopback("spoke-1"), None);
        assert!(!r.has_agent("spoke-1"));
        r.register(
            "spoke-1",
            vec![link()],
            None,
            HashMap::new(),
            Some("fd00:0:0:1::1".into()),
        );
        assert_eq!(r.loopback("spoke-1"), Some("fd00:0:0:1::1".into()));
        assert!(r.has_agent("spoke-1"));
        // Re-register without a loopback drops it (agent cannot report it).
        r.register("spoke-1", vec![link()], None, HashMap::new(), None);
        assert_eq!(r.loopback("spoke-1"), None);
    }
}
