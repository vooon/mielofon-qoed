//! Controller configuration: node identity, cluster members, listeners, TLS,
//! OTEL and quality thresholds. Mirrors the sanitized example in the handoff.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

/// Listener bind addresses. Defaults bind admin to loopback and the mTLS
/// listeners to all interfaces.
///
/// Naming follows etcd: the cluster endpoint carries node-to-node gossip
/// (the old `members` listener), the client endpoint serves agents.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Listeners {
    /// etcd-style cluster endpoint (node-to-node gossip over mTLS).
    #[serde(alias = "members_addr")]
    pub cluster_addr: IpAddr,
    #[serde(alias = "members_port")]
    pub cluster_port: u16,
    /// etcd-style client endpoint (agent API over mTLS).
    #[serde(alias = "clients_addr")]
    pub client_addr: IpAddr,
    #[serde(alias = "clients_port")]
    pub client_port: u16,
    pub admin_addr: IpAddr,
    pub admin_port: u16,
}

impl Default for Listeners {
    fn default() -> Self {
        Listeners {
            cluster_addr: "0.0.0.0".parse().unwrap(),
            cluster_port: 9551,
            client_addr: "0.0.0.0".parse().unwrap(),
            client_port: 9552,
            admin_addr: "127.0.0.1".parse().unwrap(),
            admin_port: 9553,
        }
    }
}

impl Listeners {
    pub fn cluster(&self) -> SocketAddr {
        SocketAddr::new(self.cluster_addr, self.cluster_port)
    }
    pub fn client(&self) -> SocketAddr {
        SocketAddr::new(self.client_addr, self.client_port)
    }
    pub fn admin(&self) -> SocketAddr {
        SocketAddr::new(self.admin_addr, self.admin_port)
    }
}

/// Node identity: name (placeholder) and advertise address used by peers.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Node {
    pub name: String,
    pub advertise: IpAddr,
}

impl Default for Node {
    fn default() -> Self {
        Node {
            name: "hub-a".into(),
            advertise: "203.0.113.1".parse().unwrap(),
        }
    }
}

/// TLS material paths (server+client cert per node, CA to pin).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Tls {
    pub ca: String,
    pub cert: String,
    pub key: String,
}

impl Default for Tls {
    fn default() -> Self {
        Tls {
            ca: "/etc/mielofon/ca.pem".into(),
            cert: "/etc/mielofon/node.pem".into(),
            key: "/etc/mielofon/node.key".into(),
        }
    }
}

/// Cluster membership. Keys are node placeholder names, values advertise
/// addresses. The fabric runs over the operator WAN, not the mesh underlay.
pub type Members = BTreeMap<String, IpAddr>;

/// One quality class. Each dimension is optional: an unset threshold does not
/// constrain that dimension. `rtt_ms`/`loss_pct` are upper bounds (lower is
/// better); `rr_tps`/`tcp_mbps` are lower bounds (higher is better). `ospf_cost`
/// is the metric advertised for links classified into this class.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QualityClass {
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub rr_tps: Option<f64>,
    pub tcp_mbps: Option<f64>,
    pub ospf_cost: u32,
}

impl QualityClass {
    fn with(rtt_ms: f64, loss_pct: f64, rr_tps: f64, tcp_mbps: f64, ospf_cost: u32) -> Self {
        QualityClass {
            rtt_ms: Some(rtt_ms),
            loss_pct: Some(loss_pct),
            rr_tps: Some(rr_tps),
            tcp_mbps: Some(tcp_mbps),
            ospf_cost,
        }
    }
}

/// Quality classification. A measurement is assigned the worst class whose
/// threshold it crosses; only dims listed per class take part. Conservative
/// defaults (per handoff): good/acceptable/poor/bad with increasing costs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Quality {
    pub good: QualityClass,
    pub acceptable: QualityClass,
    pub poor: QualityClass,
    pub bad: QualityClass,
}

impl Default for Quality {
    fn default() -> Self {
        Quality {
            good: QualityClass::with(40.0, 1.0, 50.0, 10.0, 10),
            acceptable: QualityClass::with(90.0, 2.5, 35.0, 5.0, 20),
            poor: QualityClass::with(250.0, 5.0, 20.0, 2.0, 50),
            bad: QualityClass::with(500.0, 10.0, 10.0, 1.0, 100),
        }
    }
}

/// Cluster-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Cluster {
    pub grace_ttl_secs: u64,
    /// Interval between gossip anti-entropy pushes to peers.
    pub gossip_interval_secs: u64,
}

impl Default for Cluster {
    fn default() -> Self {
        Cluster {
            grace_ttl_secs: 300,
            gossip_interval_secs: 5,
        }
    }
}

/// Top-level controller configuration.
///
/// ```toml
/// [node]
/// name = "hub-a"
/// advertise = "203.0.113.1"
/// [members]
/// "hub-a" = "203.0.113.1"
/// [listeners]
/// [tls]
/// [quality]
/// [otel]
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub node: Node,
    pub listeners: Listeners,
    pub cluster: Cluster,
    pub members: Members,
    pub tls: Tls,
    pub quality: Quality,
    pub otel: mielofon_otel::OTelConfig,
}

impl Config {
    /// Load from a TOML file.
    pub fn load(path: &str) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read config {}: {}", path, e))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse config {}: {}", path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.listeners.cluster_port, 9551);
        assert_eq!(c.listeners.client_port, 9552);
        assert!(c.listeners.admin_addr.is_loopback());
        assert_eq!(c.listeners.admin_port, 9553);
    }
}
