//! Gossip / anti-entropy. Each node periodically pushes its KV view to peers
//! and accepts full-view exchanges. LWW merge means last-writer-wins; the
//! store tolerates eventual convergence (no consensus required).
//!
//! Alongside the KV, each exchange carries the node's new time-series samples
//! (`ts_samples`, a bounded delta since the per-peer watermark). Samples are
//! idempotent on the receiver (per-link timestamps are monotonic), so resends
//! after a dropped round are harmless.

use crate::model::{LinkKey, QualityRecord};
use crate::state::AppState;
use crate::tsdb::{RawSample, DELTA_LIMIT};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

/// Anti-entropy exchange request: the sender's full KV view + its fresh
/// time-series samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeReq {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
    #[serde(default)]
    pub ts_samples: Vec<(LinkKey, RawSample)>,
}

#[derive(Debug, Serialize)]
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
    // Inbound push proves round-trip reachability to this peer as well.
    state.peers.mark_ok(&req.node, 0);
    trace_gossip(&req.node, req.records.len());

    // Reply with our KV view + our fresh time-series delta for this peer.
    let (ts_samples, last) = take_for_peer(&state, &req.node);
    if !ts_samples.is_empty() {
        state.tsdb.note_sent(&req.node, last);
    }
    let view = state.kv.all();
    Ok(Json(ExchangeResp {
        node: state.cfg.node.name.clone(),
        records: view,
        ts_samples,
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

/// Serialize the current KV view to bytes (for the outgoing gossip push).
pub fn encode_view(state: &AppState) -> Vec<u8> {
    let resp = ExchangeResp {
        node: state.cfg.node.name.clone(),
        records: state.kv.all(),
        ts_samples: Vec::new(),
    };
    serde_json::to_vec(&resp).unwrap_or_default()
}

/// Push our view to all cluster members over their members listener.
pub async fn gossip_loop(state: AppState, client: Arc<rustls::ClientConfig>) {
    let interval = state.cfg.cluster.gossip_interval_secs.max(1);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
    loop {
        tick.tick().await;
        let MyView { node, records } = snapshot(&state);
        for (name, addr) in &state.cfg.members {
            if name == &state.cfg.node.name {
                continue;
            }
            let (ts_samples, last) = take_for_peer(&state, name);
            let req = ExchangeReq {
                node: node.clone(),
                records: records.clone(),
                ts_samples,
            };
            let body = match serde_json::to_vec(&req) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let url_path = "/v1/gossip/exchange".to_string();
            let started = std::time::Instant::now();
            match crate::remote::post(
                *addr,
                state.cfg.listeners.cluster_port,
                client.clone(),
                &url_path,
                &body,
            )
            .await
            {
                Ok(()) => {
                    // The round succeeded: this peer has the samples.
                    state.tsdb.note_sent(name, last);
                    state
                        .peers
                        .mark_ok(name, started.elapsed().as_millis() as u64);
                }
                // Tolerate a flapping fabric: the next interval retries (the
                // watermark is untouched, so everything is re-sent).
                Err(e) => warn!("gossip push to {name}: {e}"),
            }
        }
    }
}

struct MyView {
    node: String,
    records: Vec<(LinkKey, QualityRecord)>,
}

fn snapshot(state: &AppState) -> MyView {
    MyView {
        node: state.cfg.node.name.clone(),
        records: state.kv.all(),
    }
}

/// Record a gossip ping/trace for observability (placeholder hook).
pub fn trace_gossip(node: &str, n: usize) {
    info!(target: "mielofon::gossip", peer = node, records = n, "anti-entropy exchange");
}
