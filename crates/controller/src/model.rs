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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeState {
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

/// A single link measurement, keyed by LinkKey (LWW by `ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityRecord {
    pub ts: u64,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub rr_tps: f64,
    pub tcp_mbps: Option<f64>,
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
        rtt_ms: f64,
        loss_pct: f64,
        rr_tps: f64,
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
