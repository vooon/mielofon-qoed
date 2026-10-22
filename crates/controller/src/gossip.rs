//! Gossip / anti-entropy. Each node periodically pushes its KV view to peers
//! and accepts full-view exchanges. LWW merge means last-writer-wins; the
//! store tolerates eventual convergence (no consensus required).
//!
//! Alongside the KV, each push carries the node's new time-series samples
//! (`ts_samples`, a bounded delta since the per-peer watermark). Samples
//! travel only on the push path, and the watermark advances only once the
//! peer has acknowledged the round, so a dropped round is simply re-sent.
//! Samples are idempotent on the receiver (per-link timestamps are
//! monotonic), so resends are harmless. The exchange response carries the
//! responder's KV view, which the pusher merges as well.

use crate::model::{LinkKey, QualityRecord};
use crate::state::AppState;
use crate::tsdb::{RawSample, DELTA_LIMIT};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Bound on one push (connect + handshake + exchange), so a black-holed
/// member never stalls the round for the other members.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Anti-entropy exchange request: the sender's full KV view + its fresh
/// time-series samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeReq {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
    #[serde(default)]
    pub ts_samples: Vec<(LinkKey, RawSample)>,
}

/// The responder's KV view. `ts_samples` is kept for peers running an older
/// release that still answers with samples; this node never fills it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeResp {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
    #[serde(default)]
    pub ts_samples: Vec<(LinkKey, RawSample)>,
}

/// Merge a peer's view into our store and reply with our own view.
pub async fn exchange(
    State(state): State<AppState>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<ExchangeResp>, StatusCode> {
    state.kv.merge(&req.records);
    if !req.ts_samples.is_empty() {
        state.tsdb.merge_delta(&req.ts_samples);
    }
    // Inbound push proves reachability from this peer (not a round-trip we
    // timed, so the measured rtt from our own pushes is kept).
    state.peers.mark_seen(&req.node);
    trace_gossip(&req.node, req.records.len());

    Ok(Json(ExchangeResp {
        node: state.cfg.node.name.clone(),
        records: state.kv.all(),
        ts_samples: Vec::new(),
    }))
}

/// Samples newer than the node's per-peer watermark, without advancing it.
/// Callers advance the watermark (`note_sent`) only once the round succeeds.
fn take_for_peer(state: &AppState, peer: &str) -> (Vec<(LinkKey, RawSample)>, u64) {
    let wm = state.tsdb.sent_watermark(peer);
    let (delta, last) = state.tsdb.take_delta(wm, DELTA_LIMIT);
    let samples = delta.into_iter().map(|(_, link, s)| (link, s)).collect();
    (samples, last)
}

/// Push our view to all cluster members over their members listener. Members
/// are pushed concurrently; each push is bounded by `PUSH_TIMEOUT`.
pub async fn gossip_loop(state: AppState, client: Arc<rustls::ClientConfig>) {
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
                client.clone(),
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
    client: Arc<rustls::ClientConfig>,
    name: String,
    addr: std::net::IpAddr,
    records: Arc<Vec<(LinkKey, QualityRecord)>>,
) {
    let (ts_samples, last) = take_for_peer(&state, &name);
    let req = ExchangeReq {
        node: state.cfg.node.name.clone(),
        records: records.as_ref().clone(),
        ts_samples,
    };
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            warn!("gossip encode for {name}: {e}");
            return;
        }
    };
    let started = std::time::Instant::now();
    let resp = crate::remote::post(
        addr,
        state.cfg.listeners.cluster_port,
        client,
        "/v1/gossip/exchange",
        &body,
        PUSH_TIMEOUT,
    )
    .await;
    match resp {
        Ok(bytes) => {
            // The round succeeded: this peer has the samples.
            state.tsdb.note_sent(&name, last);
            state
                .peers
                .mark_ok(&name, started.elapsed().as_millis() as u64);
            match serde_json::from_slice::<ExchangeResp>(&bytes) {
                Ok(view) => {
                    state.kv.merge(&view.records);
                    if !view.ts_samples.is_empty() {
                        state.tsdb.merge_delta(&view.ts_samples);
                    }
                }
                Err(e) => warn!("gossip reply from {name}: {e}"),
            }
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
