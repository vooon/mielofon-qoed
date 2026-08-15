//! Gossip / anti-entropy. Each node periodically pushes its KV view to peers
//! and accepts full-view exchanges. LWW merge means last-writer-wins; the
//! store tolerates eventual convergence (no consensus required).

use crate::model::{LinkKey, QualityRecord};
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

/// Anti-entropy exchange request: the sender's full KV view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeReq {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
}

#[derive(Debug, Serialize)]
pub struct ExchangeResp {
    pub node: String,
    pub records: Vec<(LinkKey, QualityRecord)>,
}

/// Merge a peer's view into our store and reply with our own view.
pub async fn exchange(
    State(state): State<AppState>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<ExchangeResp>, StatusCode> {
    state.kv.merge(&req.records);
    trace_gossip(&req.node, req.records.len());
    let view = state.kv.all();
    Ok(Json(ExchangeResp {
        node: state.cfg.node.name.clone(),
        records: view,
    }))
}

/// Serialize the current KV view to bytes (for the outgoing gossip push).
pub fn encode_view(state: &AppState) -> Vec<u8> {
    let resp = ExchangeResp {
        node: state.cfg.node.name.clone(),
        records: state.kv.all(),
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
        let req = ExchangeReq { node, records };
        for (name, addr) in &state.cfg.members {
            if name == &state.cfg.node.name {
                continue;
            }
            let body = match serde_json::to_vec(&req) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let url_path = "/v1/gossip/exchange".to_string();
            if let Err(e) = crate::remote::post(
                *addr,
                state.cfg.listeners.members_port,
                client.clone(),
                &url_path,
                &body,
            )
            .await
            {
                // Tolerate a flapping fabric: the next interval retries.
                warn!("gossip push to {name}: {e}");
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
