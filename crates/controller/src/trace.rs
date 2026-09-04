//! End-to-end, ECMP-aware path trace (controller side).
//!
//! The controller asks each hop agent (via a `WorkCmd::Trace`) to resolve a
//! destination prefix against BIRD (`rpcd-mod-bird route`) and walks the
//! returned `next_hops`, expanding every equal-cost branch into a DAG:
//!
//! ```text
//!   spoke-1 ── awg_hub_a ──> hub-a ── awg_hub_b ──> hub-b   (dev dummy_awg → reached)
//!                      └──> hub-x ──awg_xx──> xx            (dead-end: no route)
//! ```
//!
//! `kind: dev` on the destination's own loopback terminates a branch; `kind:
//! via` names the egress interface of the current hop, and the peer is resolved
//! from the link records the hop agent registered (`link.interface` is always
//! the *from* side, matching BIRD's egress). Quality/RTT/loss/cost for an edge
//! come from the controller's KV. Results are TTL-cached (~5 s) so the map
//! page's auto-refresh does not hammer the mesh.

use crate::model::{LinkKey, ProbeState, Quality};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::oneshot;

const HOP_CAP: usize = 24;
const HOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);
const CACHE_MAX: usize = 32;

/// Failure to *start* a trace (bad args, unknown source). Mid-walk problems are
/// rendered per-edge (`broken` + `reason`), not as endpoint errors.
#[derive(Debug)]
pub enum TraceError {
    BadRequest(String),
}

impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceError::BadRequest(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for TraceError {}

/// One BIRD route from `rpcd-mod-bird route` (passed through untouched — the
/// controller owns ECMP parsing). Field names pin the feed's contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRep {
    pub prefix: String,
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_pref: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_path: Option<String>,
    #[serde(default)]
    pub next_hops: Vec<NextHopRep>,
}

/// One ECMP next hop. `via addr on iface` = continue into the mesh; `dev iface`
/// = the prefix is attached locally (on the destination this is `dummy_awg`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NextHopRep {
    Via {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iface: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proto: Option<String>,
    },
    Dev {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iface: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proto: Option<String>,
    },
}

/// What the agent answers for one dispatched `trace` command.
#[derive(Debug, Clone)]
pub struct TraceHopReply {
    pub agent: String,
    pub prefix: String,
    pub code: i64,
    pub routes: Vec<RouteRep>,
}

/// One row of the trace DAG. An ECMP fan-out produces several rows at the same
/// depth from the same node; a terminal `dev` hop adds a `term` row.
#[derive(Debug, Clone, Serialize)]
pub struct TraceEdge {
    /// BFS depth (0 = the source agent itself).
    pub depth: usize,
    /// The node whose BIRD was asked to resolve the prefix.
    pub node: String,
    /// Egress interface (None for the source's own row-less hops or broken rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    /// The resolved peer this edge leads to (None on dead-ends).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// True when this row marks "destination reached" (`dev` on the target).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub term: bool,
    /// True for a conflict/bad/missing-quality edge, a dead-end (no route), a
    /// hop that timed out, or a hop-cap cut.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub broken: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Latest controller-side measurement for the edge's real link, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ospf_cost: Option<u32>,
}

impl TraceEdge {
    fn plain(depth: usize, node: &str) -> Self {
        TraceEdge {
            depth,
            node: node.to_string(),
            iface: None,
            to: None,
            term: false,
            broken: false,
            reason: None,
            rtt_ms: None,
            loss_pct: None,
            quality: None,
            ospf_cost: None,
        }
    }

    fn broken(depth: usize, node: &str, reason: impl Into<String>) -> Self {
        let mut e = Self::plain(depth, node);
        e.broken = true;
        e.reason = Some(reason.into());
        e
    }
}

/// Full trace result.
#[derive(Debug, Clone, Serialize)]
pub struct TraceResult {
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The prefix the walker resolves/discovers (loopback of `to`, or the raw
    /// `prefix` query param).
    pub prefix: String,
    /// Unix seconds when the trace ran.
    pub ts: u64,
    /// True when at least one branch reached the destination (`dev`).
    pub complete: bool,
    /// True when the hop cap cut the walk short (some branches unvisited).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cap_hit: bool,
    pub edges: Vec<TraceEdge>,
}

/// A trace request, from `GET /v1/trace`.
#[derive(Debug, Clone)]
pub struct TraceRequest {
    pub from: String,
    pub to: Option<String>,
    pub prefix: Option<String>,
}

/// Per-trace bookkeeping: pending one-shot reply channels (keyed by command
/// id, filled by `/v1/agent/reply`) and the result TTL cache.
#[derive(Default)]
pub struct TraceRegistry {
    pending: Mutex<HashMap<String, oneshot::Sender<TraceHopReply>>>,
    cache: Mutex<Vec<(String, Instant, TraceResult)>>,
}

impl TraceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for a reply to command `id`. The reply is delivered
    /// by `AgentReply::Trace` in `api::agent_reply`.
    pub fn register_pending(
        &self,
        id: &str,
    ) -> Result<oneshot::Receiver<TraceHopReply>, TraceError> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("trace pending lock poisoned")
            .insert(id.to_string(), tx);
        Ok(rx)
    }

    /// Complete a pending trace command reply (called from the agent reply
    /// path). Returns true when a waiter was found.
    pub fn complete(&self, id: &str, reply: TraceHopReply) -> bool {
        if let Some(tx) = self
            .pending
            .lock()
            .expect("trace pending lock poisoned")
            .remove(id)
        {
            let _ = tx.send(reply);
            return true;
        }
        false
    }

    /// Drop a waiter (reply timed out / agent gone).
    fn drop_pending(&self, id: &str) {
        self.pending
            .lock()
            .expect("trace pending lock poisoned")
            .remove(id);
    }

    fn cache_get(&self, from: &str, prefix: &str) -> Option<TraceResult> {
        let mut cache = self.cache.lock().expect("trace cache lock poisoned");
        let now = Instant::now();
        cache.retain(|(_, at, _)| now.duration_since(*at) <= CACHE_TTL);
        cache
            .iter()
            .find(|(f, _, r)| f == from && r.prefix == prefix)
            .map(|(_, _, r)| r.clone())
    }

    fn cache_put(&self, from: &str, result: &TraceResult) {
        let mut cache = self.cache.lock().expect("trace cache lock poisoned");
        if cache.len() >= CACHE_MAX {
            // Drop the oldest entry — Vec is insertion-ordered.
            cache.remove(0);
        }
        cache.push((from.to_string(), Instant::now(), result.clone()));
    }
}

/// Run `req` against the mesh, using the short TTL cache for repeats.
pub async fn run_trace(state: &AppState, req: TraceRequest) -> Result<TraceResult, TraceError> {
    if req.from.is_empty() {
        return Err(TraceError::BadRequest("missing from".into()));
    }
    if !state.workers.has_agent(&req.from) {
        return Err(TraceError::BadRequest(format!(
            "unknown source agent '{}' (not registered)",
            req.from
        )));
    }

    let prefix = match &req.prefix {
        Some(p) if !p.is_empty() => p.clone(),
        _ => match &req.to {
            Some(to) if !to.is_empty() => match state.workers.loopback(to) {
                Some(lb) => lb,
                None => {
                    return Err(TraceError::BadRequest(format!(
                        "cannot resolve '{}' to a prefix (agent never reported a loopback)",
                        to
                    )))
                }
            },
            _ => return Err(TraceError::BadRequest("missing to or prefix".into())),
        },
    };

    if let Some(hit) = state.trace.cache_get(&req.from, &prefix) {
        return Ok(hit);
    }

    let span = tracing::info_span!("trace.walk", from = %req.from, target = %prefix);
    let _g = span.enter();
    let result = walk(state, &req.from, req.to.clone(), &prefix).await;
    drop(_g);

    state.trace.cache_put(&req.from, &result);
    Ok(result)
}

/// BFS over the mesh's per-hop route resolutions. Never recurses and awaits each
/// hop's reply before expanding, so at most one `trace` command per target is
/// in flight and the agent queues cannot be flooded.
async fn walk(state: &AppState, from: &str, to: Option<String>, prefix: &str) -> TraceResult {
    let mut edges: Vec<TraceEdge> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(from.to_string());
    let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
    frontier.push_back((from.to_string(), 0));

    let mut complete = false;
    let mut cap_hit = false;

    while let Some((node, depth)) = frontier.pop_front() {
        if depth >= HOP_CAP {
            cap_hit = true;
            edges.push(TraceEdge::broken(
                depth,
                &node,
                format!("hop cap ({HOP_CAP}) reached"),
            ));
            continue;
        }

        let reply = match dispatch_hop(state, &node, prefix).await {
            Ok(r) => r,
            Err(reason) => {
                edges.push(TraceEdge::broken(depth, &node, reason));
                continue;
            }
        };

        let mut expanded = false;
        for nh in all_next_hops(&reply.routes) {
            expanded = true;
            match nh {
                NextHopRep::Dev { .. } => {
                    // `dev` on the destination's own loopback: reached.
                    complete = true;
                    let mut e = TraceEdge::plain(depth, &node);
                    e.iface = Some("dummy_awg".into());
                    e.to = Some(node.clone());
                    e.term = true;
                    edges.push(e);
                }
                NextHopRep::Via { iface, .. } => {
                    let iface = iface.filter(|s| !s.is_empty());
                    let peer = iface
                        .as_deref()
                        .and_then(|i| peer_for_egress(state, &node, i));
                    let m = edge_metrics(state, &node, &peer, iface.as_deref());

                    let mut e = TraceEdge::plain(depth, &node);
                    e.iface = iface.clone();
                    e.to = peer.clone();
                    e.broken = m.broken;
                    e.reason = m.reason;
                    e.rtt_ms = m.rtt_ms;
                    e.loss_pct = m.loss_pct;
                    e.quality = m.quality;
                    e.ospf_cost = m.ospf_cost;
                    edges.push(e);

                    // Expand to the peer unless we loop back or hit the cap.
                    if let Some(p) = peer {
                        if !visited.contains(&p) {
                            visited.insert(p.clone());
                            frontier.push_back((p, depth + 1));
                        }
                    }
                }
            }
        }

        if !expanded {
            edges.push(TraceEdge::broken(
                depth,
                &node,
                if reply.routes.is_empty() {
                    "unresolvable".to_string()
                } else {
                    "no usable next hop".to_string()
                },
            ));
        }
    }

    TraceResult {
        from: from.to_string(),
        to,
        prefix: prefix.to_string(),
        ts: now_secs(),
        complete,
        cap_hit,
        edges,
    }
}

/// All next hops across every returned route, deduped by (kind, iface, addr).
/// Dedup matters: ECMP routes and covering routes can repeat the same next hop.
fn all_next_hops(routes: &[RouteRep]) -> Vec<NextHopRep> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in routes {
        for nh in &r.next_hops {
            let key = match nh {
                NextHopRep::Dev { iface, .. } => format!("dev/{}", iface.as_deref().unwrap_or("")),
                NextHopRep::Via { addr, iface, .. } => {
                    format!(
                        "via/{}/{}",
                        addr.as_deref().unwrap_or(""),
                        iface.as_deref().unwrap_or("")
                    )
                }
            };
            if seen.insert(key) {
                out.push(nh.clone());
            }
        }
    }
    out
}

/// Dispatch one `trace` command to `agent` and await its reply.
async fn dispatch_hop(
    state: &AppState,
    agent: &str,
    target: &str,
) -> Result<TraceHopReply, String> {
    if !state.workers.has_agent(agent) {
        return Err(format!("agent '{agent}' is not registered"));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let rx = state
        .trace
        .register_pending(&id)
        .map_err(|_| "internal".to_string())?;

    let cmd = crate::worker::WorkCmd::Trace {
        id: id.clone(),
        target: target.to_string(),
        traceparent: None,
    };
    if !state.workers.push(agent, cmd) {
        state.trace.drop_pending(&id);
        return Err(format!("cannot queue trace for '{agent}'"));
    }

    let span = tracing::info_span!("trace.hop", agent = %agent, target = %target);
    let _g = span.enter();
    match tokio::time::timeout(HOP_TIMEOUT, rx).await {
        Ok(Ok(reply)) => {
            tracing::info!(
                agent = %agent,
                code = reply.code,
                routes = reply.routes.len(),
                "trace hop resolved"
            );
            Ok(reply)
        }
        Ok(Err(_)) | Err(_) => {
            state.trace.drop_pending(&id);
            Err(format!("hop '{agent}' unanswered (timeout)"))
        }
    }
}

/// The registered peer reachable from `node` over egress `iface`. Link records'
/// `interface` is always the *from* side, matching the hop's BIRD egress.
fn peer_for_egress(state: &AppState, node: &str, iface: &str) -> Option<String> {
    state
        .workers
        .owned_links()
        .into_iter()
        .find(|(agent, l)| agent == node && l.interface == iface)
        .map(|(_, l)| l.to)
}

/// Controller-side KV metrics for the edge `node --iface--> peer`, plus whether
/// the edge counts as broken (conflict / bad / missing quality / no record).
/// Controller-side KV metrics for one edge, plus whether it counts as broken.
struct EdgeMetrics {
    broken: bool,
    reason: Option<String>,
    rtt_ms: Option<f64>,
    loss_pct: Option<f64>,
    quality: Option<String>,
    ospf_cost: Option<u32>,
}

fn edge_metrics(
    state: &AppState,
    node: &str,
    peer: &Option<String>,
    iface: Option<&str>,
) -> EdgeMetrics {
    let (peer, iface) = match (peer, iface) {
        (Some(p), Some(i)) => (p, i),
        _ => {
            return EdgeMetrics {
                broken: true,
                reason: Some("peer not resolvable for egress interface".into()),
                rtt_ms: None,
                loss_pct: None,
                quality: None,
                ospf_cost: None,
            }
        }
    };
    match state.kv.get(&LinkKey::new(node, peer, iface)) {
        Some(rec) => {
            let broken = matches!(rec.state, ProbeState::Conflict)
                || rec.quality == Some(Quality::Bad)
                || rec.quality.is_none();
            let reason = if rec.quality.is_none() {
                Some("no quality data".into())
            } else {
                None
            };
            EdgeMetrics {
                broken,
                reason,
                rtt_ms: rec.rtt_ms,
                loss_pct: rec.loss_pct,
                quality: rec.quality.map(|q| format!("{:?}", q).to_lowercase()),
                ospf_cost: rec.ospf_cost,
            }
        }
        None => EdgeMetrics {
            broken: true,
            reason: Some("no probe data for edge".into()),
            rtt_ms: None,
            loss_pct: None,
            quality: None,
            ospf_cost: None,
        },
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::QualityRecord;
    use crate::worker::WorkCmd;

    /// Register `agents`: (name, loopback, owned links).
    fn stub_state(agents: &[(&str, Option<&str>, &[LinkKey])]) -> AppState {
        let state = AppState::new(Config::default());
        for (name, lb, links) in agents {
            state
                .workers
                .register(name, links.to_vec(), lb.map(str::to_string));
        }
        state
    }

    /// Like `stub_state` but with owned names (for generated fan-out chains).
    fn chain_state(agents: Vec<(String, Option<String>, Vec<LinkKey>)>) -> AppState {
        let state = AppState::new(Config::default());
        for (name, lb, links) in agents {
            state.workers.register(&name, links, lb);
        }
        state
    }

    fn seed(state: &AppState, from: &str, to: &str, iface: &str, quality: Quality) {
        let rec = QualityRecord {
            ts: 1_700_000_000,
            rtt_ms: Some(9.0),
            loss_pct: Some(0.0),
            rr_tps: Some(99.0),
            udp_mbps: None,
            util_mbps: 0.0,
            state: ProbeState::Quiet,
            quality: Some(quality),
            ospf_cost: Some(10),
            tcp_mbps: Some(50.0),
        };
        state.kv.put(LinkKey::new(from, to, iface), rec);
    }

    /// Drive the walker from a fake BIRD route table, completing each queued
    /// `trace` command like `/v1/agent/reply` would. `to` is the destination
    /// node name the request resolves via the registry.
    async fn drive(
        state: &AppState,
        to: &str,
        route: impl Fn(&str, &str) -> TraceHopReply + Send + Sync + 'static,
    ) -> TraceResult {
        let cmd = TraceRequest {
            from: "spoke-1".into(),
            to: Some(to.to_string()),
            prefix: None,
        };
        let walker = tokio::spawn({
            let state = state.clone();
            async move { run_trace(&state, cmd).await }
        });

        loop {
            let drained: Vec<(String, Vec<WorkCmd>)> = state
                .workers
                .agents()
                .into_iter()
                .map(|n| (n.clone(), state.workers.drain(&n)))
                .filter(|(_, c)| !c.is_empty())
                .collect();

            if drained.is_empty() {
                if walker.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
                continue;
            }

            for (agent, cmds) in drained {
                for cmd in cmds {
                    if let WorkCmd::Trace { id, target, .. } = cmd {
                        state.trace.complete(&id, route(&agent, &target));
                    }
                }
            }
        }

        walker
            .await
            .expect("walker task panicked")
            .expect("trace failed")
    }

    #[tokio::test]
    async fn happy_path_terminates_on_dev() {
        let state = stub_state(&[
            (
                "spoke-1",
                Some("fd00:0:0:1::1"),
                &[LinkKey::new("spoke-1", "hub-a", "awg_hub_a")],
            ),
            (
                "hub-a",
                Some("fd00:0:0:1::2"),
                &[
                    LinkKey::new("hub-a", "spoke-1", "awg_spoke_1"),
                    LinkKey::new("hub-a", "node-z", "awg_node_z"),
                ],
            ),
            (
                "node-z",
                Some("fd00:0:0:1::3"),
                &[LinkKey::new("node-z", "hub-a", "awg_hub_a")],
            ),
        ]);
        seed(&state, "spoke-1", "hub-a", "awg_hub_a", Quality::Good);

        let route = |agent: &str, _target: &str| TraceHopReply {
            agent: agent.into(),
            prefix: "fd00:0:0:1::3".into(),
            code: 0,
            routes: match agent {
                "spoke-1" => vec![RouteRep {
                    prefix: "fd00:0:0:1::3/128".into(),
                    primary: Some("via".into()),
                    proto: None,
                    preference: None,
                    cost: Some(10),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![NextHopRep::Via {
                        addr: Some("fd00:0:0:1::0:2".into()),
                        iface: Some("awg_hub_a".into()),
                        cost: Some(10),
                        proto: None,
                    }],
                }],
                "hub-a" => vec![RouteRep {
                    prefix: "fd00:0:0:1::3/128".into(),
                    primary: Some("via".into()),
                    proto: None,
                    preference: None,
                    cost: Some(20),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![NextHopRep::Via {
                        addr: Some("fd00:0:0:1::3".into()),
                        iface: Some("awg_node_z".into()),
                        cost: Some(20),
                        proto: None,
                    }],
                }],
                "node-z" => vec![RouteRep {
                    prefix: "fd00:0:0:1::3/128".into(),
                    primary: Some("dev".into()),
                    proto: None,
                    preference: None,
                    cost: Some(0),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![NextHopRep::Dev {
                        iface: Some("dummy_awg".into()),
                        cost: Some(0),
                        proto: None,
                    }],
                }],
                _ => unreachable!(),
            },
        };

        let res = drive(&state, "node-z", route).await;
        assert!(res.complete, "trace must reach the destination");
        assert!(!res.cap_hit);

        let dep: Vec<&str> = res.edges.iter().map(|e| e.node.as_str()).collect();
        assert_eq!(dep, ["spoke-1", "hub-a", "node-z"]);

        let via = &res.edges[0];
        assert_eq!(via.iface.as_deref(), Some("awg_hub_a"));
        assert_eq!(via.to.as_deref(), Some("hub-a"));
        assert!(!via.broken);
        assert_eq!(via.rtt_ms, Some(9.0));
        assert_eq!(via.quality.as_deref(), Some("good"));
        assert_eq!(via.ospf_cost, Some(10));

        let term = &res.edges[2];
        assert!(term.term);
        assert_eq!(term.node, "node-z");
    }

    #[tokio::test]
    async fn ecmp_fanout_and_dead_end() {
        let state = stub_state(&[
            (
                "spoke-1",
                Some("fd00:0:0:1::1"),
                &[LinkKey::new("spoke-1", "hub-a", "awg_hub_a")],
            ),
            (
                "hub-a",
                Some("fd00:0:0:1::2"),
                &[
                    LinkKey::new("hub-a", "spoke-1", "awg_spoke_1"),
                    LinkKey::new("hub-a", "hub-b", "awg_hub_b"),
                    LinkKey::new("hub-a", "hub-x", "awg_hub_x"),
                ],
            ),
            (
                "hub-b",
                Some("fd00:0:0:1::3"),
                &[
                    LinkKey::new("hub-b", "hub-a", "awg_hub_a"),
                    LinkKey::new("hub-b", "node-z", "awg_node_z"),
                ],
            ),
            ("node-z", Some("fd00:0:0:1::4"), &[]),
            ("hub-x", Some("fd00:0:0:1::99"), &[]),
        ]);
        seed(&state, "hub-a", "hub-b", "awg_hub_b", Quality::Acceptable);

        let route = |agent: &str, _t: &str| {
            let mut routes = if agent == "spoke-1" {
                vec![RouteRep {
                    prefix: "fd00:0:0:1::4/128".into(),
                    primary: Some("via".into()),
                    proto: None,
                    preference: None,
                    cost: Some(10),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![NextHopRep::Via {
                        addr: None,
                        iface: Some("awg_hub_a".into()),
                        cost: Some(10),
                        proto: None,
                    }],
                }]
            } else if agent == "hub-a" {
                // ECMP: two equal-cost paths to node-z.
                vec![RouteRep {
                    prefix: "fd00:0:0:1::4/128".into(),
                    primary: Some("via".into()),
                    proto: None,
                    preference: None,
                    cost: Some(20),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![
                        NextHopRep::Via {
                            addr: None,
                            iface: Some("awg_hub_b".into()),
                            cost: Some(20),
                            proto: None,
                        },
                        NextHopRep::Via {
                            addr: None,
                            iface: Some("awg_hub_x".into()),
                            cost: Some(20),
                            proto: None,
                        },
                    ],
                }]
            } else if agent == "hub-b" {
                vec![RouteRep {
                    prefix: "fd00:0:0:1::4/128".into(),
                    primary: Some("dev".into()),
                    proto: None,
                    preference: None,
                    cost: Some(0),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![NextHopRep::Dev {
                        iface: Some("dummy_awg".into()),
                        cost: Some(0),
                        proto: None,
                    }],
                }]
            } else {
                // hub-x: no route out — unresolvable dead-end, still code 0 with
                // no routes so the "no route" branch is exercised by hub-b's dev.
                vec![]
            };
            if agent == "hub-x" {
                routes = Vec::new();
            }
            TraceHopReply {
                agent: agent.into(),
                prefix: "fd00:0:0:1::4".into(),
                code: if agent == "hub-x" { 1 } else { 0 },
                routes,
            }
        };

        let res = drive(&state, "node-z", route).await;
        assert!(res.complete);

        // hub-a fans out into two ECMP edges at depth 1.
        let depth1: Vec<&TraceEdge> = res.edges.iter().filter(|e| e.depth == 1).collect();
        assert_eq!(depth1.len(), 2, "ECMP must produce two edges from hub-a");
        let e = depth1
            .iter()
            .find(|e| e.iface.as_deref() == Some("awg_hub_b"))
            .expect("primary ECMP edge");
        assert_eq!(e.to.as_deref(), Some("hub-b"));
        assert!(!e.broken);
        assert_eq!(e.ospf_cost, Some(10)); // from seeded KV

        let dead = depth1
            .iter()
            .find(|e| e.iface.as_deref() == Some("awg_hub_x"))
            .expect("secondary ECMP edge");
        assert_eq!(dead.to.as_deref(), Some("hub-x"));
        assert!(dead.broken, "dead branch should be marked broken");

        // The dead branch logs a broken row when hub-x is probed.
        let dead_row = res
            .edges
            .iter()
            .find(|e| e.node == "hub-x")
            .expect("hub-x visited");
        assert!(dead_row.broken);
    }

    #[tokio::test]
    async fn hop_cap_cuts_long_chains() {
        // spoke-1 -> node-00 -> node-01 -> ... : a chain longer than HOP_CAP.
        let mut agents: Vec<(String, Option<String>, Vec<LinkKey>)> = vec![(
            "spoke-1".into(),
            Some("fd00:0:0:1::1".into()),
            vec![LinkKey::new("spoke-1", "node-00", "awg_00")],
        )];
        for i in 0..=HOP_CAP {
            let name = format!("node-{i:02}");
            let links = if i < HOP_CAP {
                vec![LinkKey::new(
                    &name,
                    format!("node-{:02}", i + 1),
                    format!("awg_{i:02}"),
                )]
            } else {
                Vec::new()
            };
            agents.push((name, Some("fd00:0:0:1::ff".into()), links));
        }
        let state = chain_state(agents);

        let route = |agent: &str, _t: &str| {
            // spoke-1 -> node-00 -> node-01 -> … ; every node forwards deeper.
            // Map the agent back to its registered egress interface so the
            // walker can resolve the peer (peer_for_egress) and expand the BFS.
            let (num, own) = if agent == "spoke-1" {
                (0_u32, false)
            } else if let Some(rest) = agent.strip_prefix("node-") {
                let n = rest.parse::<u32>().unwrap_or(999);
                (n, true)
            } else {
                (999, false)
            };
            let _ = own;
            let iface_no = if agent == "spoke-1" { 0 } else { num };
            TraceHopReply {
                agent: agent.into(),
                prefix: "fd00:0:0:1::ff".into(),
                code: 0,
                routes: vec![RouteRep {
                    prefix: "fd00:0:0:1::ff/128".into(),
                    primary: Some("via".into()),
                    proto: None,
                    preference: None,
                    cost: Some(10),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![NextHopRep::Via {
                        addr: None,
                        iface: Some(format!("awg_{iface_no:02}")),
                        cost: Some(10),
                        proto: None,
                    }],
                }],
            }
        };

        let res = drive(&state, "node-01", route).await;
        assert!(!res.complete);
        assert!(res.cap_hit, "a long chain must end at the hop cap");
        // HOP_CAP via edges + one cap marker row.
        assert_eq!(res.edges.len(), HOP_CAP + 1);
        let cap_rows = res
            .edges
            .iter()
            .filter(|e| e.reason.as_deref().is_some_and(|r| r.contains("hop cap")));
        assert_eq!(cap_rows.count(), 1);
    }

    #[tokio::test]
    async fn routing_loop_recorded_but_not_reexpanded() {
        let state = stub_state(&[
            (
                "spoke-1",
                Some("fd00:0:0:1::1"),
                &[LinkKey::new("spoke-1", "hub-a", "awg_hub_a")],
            ),
            (
                "hub-a",
                Some("fd00:0:0:1::2"),
                &[
                    LinkKey::new("hub-a", "spoke-1", "awg_spoke_1"),
                    LinkKey::new("hub-a", "hub-b", "awg_hub_b"),
                ],
            ),
            (
                "hub-b",
                Some("fd00:0:0:1::3"),
                &[LinkKey::new("hub-b", "hub-a", "awg_hub_a")],
            ),
        ]);

        // hub-a and hub-b bounce the destination between themselves forever.
        let route = |agent: &str, _t: &str| {
            let next = if agent == "hub-a" {
                "awg_hub_b"
            } else {
                "awg_hub_a"
            };
            let hop = NextHopRep::Via {
                addr: None,
                iface: Some(next.into()),
                cost: Some(10),
                proto: None,
            };
            TraceHopReply {
                agent: agent.into(),
                prefix: "fd00:0:0:1::ff".into(),
                code: 0,
                routes: vec![RouteRep {
                    prefix: "fd00:0:0:1::ff/128".into(),
                    primary: Some("via".into()),
                    proto: None,
                    preference: None,
                    cost: Some(10),
                    local_pref: None,
                    as_path: None,
                    next_hops: vec![hop],
                }],
            }
        };
        let res = drive(&state, "hub-b", route).await;
        assert!(!res.complete);
        assert!(!res.cap_hit);
        // visited keeps the walk finite: spoke-1, hub-a, hub-b probed once each.
        let probed: HashSet<&str> = res.edges.iter().map(|e| e.node.as_str()).collect();
        assert_eq!(probed.len(), 3);
        // The loop edge back to hub-a is recorded but not re-expanded: still 3 rows.
        assert_eq!(res.edges.len(), 3);
    }
}
