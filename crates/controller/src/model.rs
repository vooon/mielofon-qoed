//! Shared value types: links, quality records, probe state.

use crate::clock::now_secs;
use serde::{Deserialize, Serialize};

/// Wire types shared with the agent.
pub use mielofon_proto::{LinkKey, ProbeState};

/// Quality class assigned by the controller from thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Good,
    Acceptable,
    Poor,
    Bad,
}

/// A per-link quality/policy record, keyed by LinkKey (LWW by `ts`).
///
/// The KV holds the *derived* record written by the classifier: the collapsed
/// measurement window (rtt/loss/jitter/tcp/util/state) plus the
/// controller-assigned `quality` and `ospf_cost`. Each dimension is optional —
/// unset dimensions never constrain classification, and the window collapse
/// (see `store.rs`) carries a sparse dimension (e.g. the gated throughput
/// probe) so it is not blanked by a more frequent probe of the others.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityRecord {
    pub ts: u64,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub jitter_ms: Option<f64>,
    #[serde(default)]
    pub tcp_mbps: Option<f64>,
    #[serde(default)]
    pub udp_mbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_jitter_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_loss_pct: Option<f64>,
    pub util_mbps: f64,
    pub state: ProbeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ospf_cost: Option<u32>,
}

impl QualityRecord {
    pub fn new(
        rtt_ms: Option<f64>,
        loss_pct: Option<f64>,
        jitter_ms: Option<f64>,
        tcp_mbps: Option<f64>,
        udp_mbps: Option<f64>,
        util_mbps: f64,
        state: ProbeState,
    ) -> Self {
        QualityRecord {
            ts: now_secs(),
            rtt_ms,
            loss_pct,
            jitter_ms,
            tcp_mbps,
            udp_mbps,
            udp_jitter_ms: None,
            udp_loss_pct: None,
            util_mbps,
            state,
            quality: None,
            ospf_cost: None,
        }
    }
}

/// A node known by router id: the name it registered under, LWW by `ts`.
/// Replicated between members like the link records, so every hub can
/// name the far end of a tunnel its agent only knows by router id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeEntry {
    pub name: String,
    pub ts: u64,
    /// The links the node registered (its side of every tunnel), so every
    /// member knows the topology before a single sample arrived.
    #[serde(default)]
    pub links: Vec<LinkKey>,
}

/// User-facing quality report (metrics + controller-assigned class/cost).
#[derive(Debug, Clone, Serialize)]
pub struct QualityView {
    pub link: LinkKey,
    #[serde(flatten)]
    pub record: QualityRecord,
}
