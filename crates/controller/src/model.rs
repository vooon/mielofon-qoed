//! Shared value types: links, quality records, probe state.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Identifies a directed link.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkKey {
    pub from: String,
    pub to: String,
    pub interface: String,
}

impl LinkKey {
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        interface: impl Into<String>,
    ) -> Self {
        LinkKey {
            from: from.into(),
            to: to.into(),
            interface: interface.into(),
        }
    }

    /// Compact stable identifier used as map keys and command ids.
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.from, self.to, self.interface)
    }
}

/// Probe state reported by the agent alongside measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProbeState {
    #[default]
    Quiet,
    Busy,
    Conflict,
}

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
/// measurement window (rtt/loss/rr/tcp/util/state) plus the controller-assigned
/// `quality` and `ospf_cost`. Each dimension is optional — unset dimensions
/// never constrain classification, and the window collapse (see `store.rs`)
/// carries a sparse dimension (e.g. the gated throughput probe) so it is not
/// blanked by a more frequent probe of the others.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityRecord {
    pub ts: u64,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub rr_tps: Option<f64>,
    #[serde(default)]
    pub tcp_mbps: Option<f64>,
    #[serde(default)]
    pub udp_mbps: Option<f64>,
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
        rr_tps: Option<f64>,
        tcp_mbps: Option<f64>,
        udp_mbps: Option<f64>,
        util_mbps: f64,
        state: ProbeState,
    ) -> Self {
        QualityRecord {
            ts: now_secs(),
            rtt_ms,
            loss_pct,
            rr_tps,
            tcp_mbps,
            udp_mbps,
            util_mbps,
            state,
            quality: None,
            ospf_cost: None,
        }
    }
}

/// User-facing quality report (metrics + controller-assigned class/cost).
#[derive(Debug, Clone, Serialize)]
pub struct QualityView {
    pub link: LinkKey,
    #[serde(flatten)]
    pub record: QualityRecord,
}
