//! Mesh map: graph data for the dashboard.
//!
//! The graph is consumed by the Vue 3 SPA (`frontend/`) via
//! `GET /v1/graph`: cluster members on a ring, spokes radiating out, edges
//! colored by quality class, broken links drawn red/dashed. Registered agents
//! appear even before their first measurement so spokes do not vanish while
//! momentarily silent.

use crate::model::{LinkKey, ProbeState, Quality};
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
    /// RTT jitter (ms), derived from the ping RTT distribution — the
    /// congestion/latency-stability signal.
    pub jitter_ms: Option<f64>,
    /// Best-effort TCP throughput (Mbps), only present for throughput probes.
    pub tcp_mbps: Option<f64>,
    /// Gated UDP leg: received rate, jitter and loss at the peer.
    pub udp_mbps: Option<f64>,
    pub udp_jitter_ms: Option<f64>,
    pub udp_loss_pct: Option<f64>,
    pub util_mbps: f64,
    pub state: String,
    pub quality: Option<String>,
    pub ospf_cost: Option<u32>,
    /// Unix seconds of the last probe.
    pub ts: u64,
}

impl GraphLink {
    /// A registered link nothing has measured yet: on the map as "no data".
    fn unmeasured(key: LinkKey) -> Self {
        GraphLink {
            from: key.from,
            to: key.to,
            interface: key.interface,
            rtt_ms: None,
            loss_pct: None,
            jitter_ms: None,
            tcp_mbps: None,
            udp_mbps: None,
            udp_jitter_ms: None,
            udp_loss_pct: None,
            util_mbps: 0.0,
            state: "quiet".into(),
            quality: None,
            ospf_cost: None,
            ts: 0,
        }
    }
}

/// Build the graph: nodes = cluster members + every from/to seen in the KV;
/// links = one per quality record.
pub fn graph_part(state: &AppState) -> (Vec<GraphNode>, Vec<GraphLink>) {
    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();

    for name in state.cfg.members.keys().filter(|n| state.cfg.is_hub(n)) {
        nodes.insert(
            name.clone(),
            GraphNode {
                id: name.clone(),
                label: name.clone(),
                group: "hub".into(),
            },
        );
    }

    // Registered but not yet measured links come from the replicated node
    // directory: the topology shows at once, as "no data" until the first
    // sample lands.
    let mut links: Vec<GraphLink> = Vec::new();
    let records = state.kv.all();
    let measured: std::collections::HashSet<LinkKey> =
        records.iter().map(|(k, _)| k.clone()).collect();
    for (_, entry) in state.kv.nodes() {
        for key in entry.links {
            if measured.contains(&key) {
                continue;
            }
            for id in [&key.from, &key.to] {
                nodes.entry(id.clone()).or_insert_with(|| GraphNode {
                    id: id.clone(),
                    label: id.clone(),
                    group: if state.cfg.is_hub(id) {
                        "hub".to_string()
                    } else {
                        "spoke".to_string()
                    },
                });
            }
            links.push(GraphLink::unmeasured(key));
        }
    }
    for (key, rec) in records {
        for id in [&key.from, &key.to] {
            nodes.entry(id.clone()).or_insert_with(|| GraphNode {
                id: id.clone(),
                label: id.clone(),
                group: if state.cfg.is_hub(id) {
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
            jitter_ms: rec.jitter_ms,
            tcp_mbps: rec.tcp_mbps,
            udp_mbps: rec.udp_mbps,
            udp_jitter_ms: rec.udp_jitter_ms,
            udp_loss_pct: rec.udp_loss_pct,
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
