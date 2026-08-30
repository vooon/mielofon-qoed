//! Shared value types (mirrors the controller's wire model).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkKey {
    pub from: String,
    pub to: String,
    pub interface: String,
}

impl LinkKey {
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.from, self.to, self.interface)
    }
}

/// Probe state reported with a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeState {
    Quiet,
    Busy,
    Conflict,
}

/// Body of the report sent to `POST /v1/quality`.
#[derive(Debug, Serialize)]
pub struct QualityReport {
    pub link: LinkKey,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub rr_tps: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_mbps: Option<f64>,
    pub util_mbps: f64,
    pub state: ProbeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Register request body.
#[derive(Debug, Serialize)]
pub struct RegisterReq {
    pub agent: String,
    pub links: Vec<LinkKey>,
}

/// Work pull request body.
#[derive(Debug, Serialize)]
pub struct WorkReq {
    pub agent: String,
}

/// Apply-ack request body.
#[derive(Debug, Serialize)]
pub struct ApplyAck {
    pub agent: String,
    pub link: LinkKey,
    pub cost: u32,
}

/// A command returned by the controller.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkCmd {
    Probe {
        id: String,
        tier: String,
        token: Option<String>,
        link: LinkKey,
    },
    ApplyCost {
        #[allow(dead_code)]
        id: String,
        link: LinkKey,
        cost: u32,
    },
}

/// Parsed wrapper for the work response.
#[derive(Debug, Deserialize)]
pub struct WorkResp {
    pub commands: Vec<WorkCmd>,
}
