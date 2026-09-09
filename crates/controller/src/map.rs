//! Mesh map: graph data for the embedded dashboard.
//!
//! The graph is consumed by the Vue 3 SPA (`crates/controller/frontend`) via
//! `GET /v1/graph`: cluster members on a ring, spokes radiating out, edges
//! colored by quality class, broken links drawn red/dashed. Registered agents
//! appear even before their first measurement so spokes do not vanish while
//! momentarily silent.

use crate::model::{ProbeState, Quality};
use crate::state::AppState;
use std::collections::BTreeMap;

/// A node in the map.
#[derive(serde::Serialize)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    /// "hub" for cluster members, "spoke" otherwise.
    pub group: String,
}

/// A directional link (agent -> peer tunnel) with the latest measurement.
#[derive(serde::Serialize)]
pub struct GraphLink {
    pub from: String,
    pub to: String,
    pub interface: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub rr_tps: Option<f64>,
    pub util_mbps: f64,
    pub state: String,
    pub quality: Option<String>,
    pub ospf_cost: Option<u32>,
    /// Unix seconds of the last probe.
    pub ts: u64,
}

/// Build the graph: nodes = cluster members + every from/to seen in the KV;
/// links = one per quality record.
pub fn graph_part(state: &AppState) -> (Vec<GraphNode>, Vec<GraphLink>) {
    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();

    for name in state.cfg.members.keys() {
        nodes.insert(
            name.clone(),
            GraphNode {
                id: name.clone(),
                label: name.clone(),
                group: "hub".into(),
            },
        );
    }

    let mut links: Vec<GraphLink> = Vec::new();
    for (key, rec) in state.kv.all() {
        for id in [&key.from, &key.to] {
            nodes.entry(id.clone()).or_insert_with(|| GraphNode {
                id: id.clone(),
                label: id.clone(),
                group: if state.cfg.members.contains_key(id) {
                    "hub".to_string()
                } else {
                    "spoke".to_string()
                },
            });
        }
        links.push(GraphLink {
            from: key.from,
            to: key.to,
            interface: key.interface,
            rtt_ms: rec.rtt_ms,
            loss_pct: rec.loss_pct,
            rr_tps: rec.rr_tps,
            util_mbps: rec.util_mbps,
            state: match rec.state {
                ProbeState::Quiet => "quiet",
                ProbeState::Busy => "busy",
                ProbeState::Conflict => "conflict",
            }
            .into(),
            quality: rec
                .quality
                .map(|q| match q {
                    Quality::Good => "good",
                    Quality::Acceptable => "acceptable",
                    Quality::Poor => "poor",
                    Quality::Bad => "bad",
                })
                .map(str::to_string),
            ospf_cost: rec.ospf_cost,
            ts: rec.ts,
        });
    }

    (nodes.into_values().collect(), links)
}
