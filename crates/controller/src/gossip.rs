//! Gossip / anti-entropy. Each node periodically pushes its KV view to peers
//! and accepts full-view exchanges. LWW merge means last-writer-wins; the
//! store tolerates eventual convergence (no consensus required).
//!
//! Members talk over the same mTLS QUIC transport as agents, on the
//! **cluster** listener. Every member keeps one outbound session per peer
//! (dialled on first use, re-dialled after a failure) and pushes on a fresh
//! stream each round; the peer answers on the same stream. Inbound sessions
//! are served symmetrically, so a pair of members holds two sessions, one
//! per direction. Nothing is negotiated on a session: the CA pins the peer,
//! the exchange names the sender.
//!
//! Alongside the KV, each push carries the node's new time-series samples
//! (`ts_samples`, a bounded delta since the per-peer watermark). Samples
//! travel only on the push path, and the watermark advances only once the
//! peer has acknowledged the round, so a dropped round is simply re-sent.
//! Samples are idempotent on the receiver (per-link timestamps are
//! monotonic), so resends are harmless. The exchange response carries the
//! responder's KV view, which the pusher merges as well.

use crate::model::{LinkKey, NodeEntry, QualityRecord};
use crate::state::AppState;
use crate::tsdb::{RawSample, DELTA_LIMIT};
use mielofon_transport::{request, respond, Dialer, Listener, QuicSession, Session};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Bound on one push (dial if needed + exchange), so a black-holed member
/// never stalls the round for the other members.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshake budget for a member that has no live session.
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// A request a member opens on a cluster session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemberRequest {
    /// Anti-entropy push: the sender's full KV view + fresh samples.
    Exchange(ExchangeReq),
}

/// The answer to a [`MemberRequest`], on the same stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemberReply {
    Exchange(ExchangeResp),
}

/// Anti-entropy exchange request: the sender's full KV view + its fresh
/// time-series samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeReq {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
    #[serde(default)]
    pub ts_samples: Vec<(LinkKey, RawSample)>,
    /// The node directory (router id → registered name), LWW-merged.
    #[serde(default)]
    pub nodes: Vec<(String, NodeEntry)>,
}

/// The responder's KV view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeResp {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
    #[serde(default)]
    pub nodes: Vec<(String, NodeEntry)>,
}

/// Merge a peer's view into our store and reply with our own view.
pub fn exchange(state: &AppState, req: ExchangeReq) -> ExchangeResp {
    state.kv.merge(&req.records);
    if !req.ts_samples.is_empty() {
        state.tsdb.merge_delta(&req.ts_samples);
    }
    learn_nodes(state, &req.nodes);
    // Inbound push proves reachability from this peer (not a round-trip we
    // timed, so the measured rtt from our own pushes is kept).
    state.peers.mark_seen(&req.node);
    trace_gossip(&req.node, req.records.len());

    ExchangeResp {
        node: state.cfg.node.name.clone(),
        records: state.kv.all(),
        nodes: state.kv.nodes(),
    }
}

/// Merge a peer's node directory and name our agents' links by it.
fn learn_nodes(state: &AppState, nodes: &[(String, NodeEntry)]) {
    if !nodes.is_empty() && state.kv.merge_nodes(nodes) {
        crate::session::sync_directory(state);
    }
}

// ── Inbound: the cluster listener ─────────────────────────────────────────

/// Accept member sessions forever. Returns only when the listener dies.
pub async fn serve(state: AppState, listener: Listener) -> anyhow::Result<()> {
    loop {
        match listener.accept().await {
            None => anyhow::bail!("cluster listener closed"),
            Some(Err(e)) => warn!("cluster session refused: {e:#}"),
            Some(Ok(session)) => {
                tokio::spawn(handle(state.clone(), Arc::new(session)));
            }
        }
    }
}

/// Serve every stream a member opens on its session until it closes.
async fn handle(state: AppState, session: Arc<dyn Session>) {
    let remote = session.remote();
    debug!(%remote, "cluster session open");
    let reason = loop {
        tokio::select! {
            accepted = session.accept() => match accepted {
                Ok(stream) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let res = respond(stream, |req: MemberRequest| {
                            let reply = match req {
                                MemberRequest::Exchange(req) => {
                                    MemberReply::Exchange(exchange(&state, req))
                                }
                            };
                            async move { Some(reply) }
                        })
                        .await;
                        if let Err(e) = res {
                            debug!(%remote, "cluster request failed: {e}");
                        }
                    });
                }
                Err(e) => break e.to_string(),
            },
            reason = session.closed() => break reason,
        }
    };
    debug!(%remote, "cluster session closed: {reason}");
}

// ── Outbound: one session per member ──────────────────────────────────────

/// Outbound member sessions, keyed by member name. A session is dialled on
/// first use and dropped after a failed exchange, so the next round
/// re-dials; a member that is down costs one `DIAL_TIMEOUT` per round.
pub struct Fabric {
    dialer: Dialer,
    sessions: Mutex<HashMap<String, Arc<QuicSession>>>,
}

impl Fabric {
    pub fn new(client_tls: Arc<rustls::ClientConfig>) -> anyhow::Result<Self> {
        Ok(Fabric {
            dialer: Dialer::new(client_tls)?,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// The live session to `name`, dialling `addr` (verified against the
    /// literal address, as listed under `[members]`) when there is none.
    async fn session(&self, name: &str, addr: SocketAddr) -> anyhow::Result<Arc<QuicSession>> {
        if let Some(s) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
        {
            return Ok(s.clone());
        }
        let session = Arc::new(
            self.dialer
                .dial(addr, &addr.ip().to_string(), DIAL_TIMEOUT)
                .await?,
        );
        info!(member = name, remote = %addr, "member session up");
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), session.clone());
        Ok(session)
    }

    /// Forget the session to `name` (closed after a failure).
    fn drop_session(&self, name: &str) {
        let old = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some(s) = old {
            s.close("exchange failed");
        }
    }

    /// Open a stream to `name`, send `req` and read the reply, dialling
    /// first when needed. The whole thing is bounded by `PUSH_TIMEOUT`.
    pub async fn exchange(
        &self,
        name: &str,
        addr: SocketAddr,
        req: &MemberRequest,
    ) -> anyhow::Result<MemberReply> {
        let run = async {
            let session = self.session(name, addr).await?;
            let reply: MemberReply = request(&*session, req, PUSH_TIMEOUT).await?;
            Ok(reply)
        };
        let res: anyhow::Result<MemberReply> = tokio::time::timeout(PUSH_TIMEOUT, run)
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "timed out after {}s",
                    PUSH_TIMEOUT.as_secs()
                ))
            });
        if res.is_err() {
            self.drop_session(name);
        }
        res
    }
}

/// Samples newer than the node's per-peer watermark, without advancing it.
/// Callers advance the watermark (`note_sent`) only once the round succeeds.
fn take_for_peer(state: &AppState, peer: &str) -> (Vec<(LinkKey, RawSample)>, u64) {
    let wm = state.tsdb.sent_watermark(peer);
    let (delta, last) = state.tsdb.take_delta(wm, DELTA_LIMIT);
    let samples = delta.into_iter().map(|(_, link, s)| (link, s)).collect();
    (samples, last)
}

/// Push our view to all cluster members over their cluster listener.
/// Members are pushed concurrently; each push is bounded by `PUSH_TIMEOUT`.
pub async fn gossip_loop(state: AppState, fabric: Arc<Fabric>) {
    let interval = state.cfg.cluster.gossip_interval_secs.max(1);
    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    loop {
        tick.tick().await;
        let records = Arc::new(state.kv.all());
        let mut rounds = Vec::new();
        for (name, addr) in &state.cfg.members {
            if name == &state.cfg.node.name {
                continue;
            }
            rounds.push(tokio::spawn(push_one(
                state.clone(),
                fabric.clone(),
                name.clone(),
                *addr,
                records.clone(),
            )));
        }
        for r in rounds {
            let _ = r.await;
        }
    }
}

async fn push_one(
    state: AppState,
    fabric: Arc<Fabric>,
    name: String,
    addr: IpAddr,
    records: Arc<Vec<(LinkKey, QualityRecord)>>,
) {
    let (ts_samples, last) = take_for_peer(&state, &name);
    let req = MemberRequest::Exchange(ExchangeReq {
        node: state.cfg.node.name.clone(),
        records: records.as_ref().clone(),
        ts_samples,
        nodes: state.kv.nodes(),
    });
    let addr = SocketAddr::new(addr, state.cfg.listeners.cluster_port);
    let started = std::time::Instant::now();
    match fabric.exchange(&name, addr, &req).await {
        Ok(MemberReply::Exchange(view)) => {
            // The round succeeded: this peer has the samples.
            state.tsdb.note_sent(&name, last);
            state
                .peers
                .mark_ok(&name, started.elapsed().as_millis() as u64);
            state.kv.merge(&view.records);
            learn_nodes(&state, &view.nodes);
        }
        // Tolerate a flapping fabric: the next interval retries (the
        // watermark is untouched, so everything is re-sent).
        Err(e) => warn!("gossip push to {name}: {e:#}"),
    }
}

/// Record a gossip exchange for observability.
pub fn trace_gossip(node: &str, n: usize) {
    info!(target: "mielofon::gossip", peer = node, records = n, "anti-entropy exchange");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::QualityRecord;

    fn node(name: &str, cluster_port: u16) -> AppState {
        let mut cfg = Config::default();
        cfg.node.name = name.into();
        cfg.listeners.cluster_port = cluster_port;
        AppState::new(cfg)
    }

    fn record(ts: u64) -> QualityRecord {
        let mut r = QualityRecord::new(
            Some(1.0),
            Some(0.0),
            None,
            None,
            None,
            0.0,
            crate::model::ProbeState::Quiet,
        );
        r.ts = ts;
        r
    }

    #[tokio::test]
    async fn members_exchange_views_over_quic_and_recover_from_a_lost_session() {
        let pki = mielofon_transport::testpki::pki();
        let listener = Listener::bind(pki.server.clone(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let port = listener.local_addr().unwrap().port();

        let a = node("hub-a", port);
        let b = node("hub-b", port);
        let link_a = LinkKey::new("node-0", "hub-a", "awg0");
        let link_b = LinkKey::new("node-1", "hub-b", "awg0");
        a.kv.merge(&[(link_a.clone(), record(10))]);
        b.kv.merge(&[(link_b.clone(), record(20))]);
        // b learned a spoke's name from its own agent; a's hub only knows
        // the router id and must get the name through gossip.
        b.kv.set_node("192.0.2.9", "node-0.example", vec![], 5);
        a.workers.register(
            "hub-a",
            vec![LinkKey::new("hub-a", "node.0.example", "awg_sp_n0")],
            Some("192.0.2.1".into()),
            [("awg_sp_n0".to_string(), "192.0.2.9".to_string())].into(),
            None,
        );
        a.tsdb.append(
            &link_a,
            RawSample::from_dims(
                crate::model::ProbeState::Quiet,
                100,
                Some(1.0),
                Some(0.0),
                None,
                None,
                0.0,
            ),
        );

        tokio::spawn(serve(b.clone(), listener));
        let fabric = Arc::new(Fabric::new(pki.client).unwrap());
        let records = Arc::new(a.kv.all());
        let peer: IpAddr = "127.0.0.1".parse().unwrap();

        push_one(
            a.clone(),
            fabric.clone(),
            "hub-b".into(),
            peer,
            records.clone(),
        )
        .await;

        // Both sides hold both records; the sample travelled on the push;
        // liveness is recorded on both ends.
        assert!(b.kv.get(&link_a).is_some(), "b merged a's record");
        assert!(a.kv.get(&link_b).is_some(), "a merged b's reply");
        assert_eq!(b.tsdb.query(&link_a, 0, 200).samples.len(), 1);
        assert!(a
            .peers
            .snapshot()
            .iter()
            .any(|(n, h)| n == "hub-b" && h.rtt_ms.is_some()));
        assert!(b
            .peers
            .snapshot()
            .iter()
            .any(|(n, h)| n == "hub-a" && h.last_ok_ts.is_some()));
        assert_eq!(a.tsdb.sent_watermark("hub-b"), 1, "watermark advanced");
        assert_eq!(
            a.workers.links_of("hub-a")[0].to,
            "node-0.example",
            "name learned through the exchange reply"
        );
        assert_eq!(
            a.kv.node("192.0.2.1").unwrap().links[0].to,
            "node-0.example",
            "hub-a's entry republished after the rename"
        );

        // The session is reused across rounds...
        let first = fabric
            .sessions
            .lock()
            .unwrap()
            .get("hub-b")
            .cloned()
            .unwrap();
        push_one(
            a.clone(),
            fabric.clone(),
            "hub-b".into(),
            peer,
            records.clone(),
        )
        .await;
        let second = fabric
            .sessions
            .lock()
            .unwrap()
            .get("hub-b")
            .cloned()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        // ...and re-dialled after the peer closes it.
        first.close("test");
        push_one(
            a.clone(),
            fabric.clone(),
            "hub-b".into(),
            peer,
            records.clone(),
        )
        .await;
        push_one(a.clone(), fabric.clone(), "hub-b".into(), peer, records).await;
        let third = fabric.sessions.lock().unwrap().get("hub-b").cloned();
        assert!(
            third.is_some_and(|s| !Arc::ptr_eq(&first, &s)),
            "fresh session"
        );
    }

    #[tokio::test]
    async fn push_to_a_dead_member_fails_fast() {
        let pki = mielofon_transport::testpki::pki();
        let a = node("hub-a", 9);
        let fabric = Arc::new(Fabric::new(pki.client).unwrap());
        let started = std::time::Instant::now();
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        push_one(
            a.clone(),
            fabric,
            "hub-b".into(),
            peer,
            Arc::new(Vec::new()),
        )
        .await;
        assert!(started.elapsed() < PUSH_TIMEOUT);
        assert!(a.peers.snapshot().is_empty());
        assert_eq!(a.tsdb.sent_watermark("hub-b"), 0);
    }
}
