//! Controller configuration: node identity, cluster members, listeners, TLS,
//! OTEL and quality thresholds. Mirrors the sanitized example in the handoff.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

/// Listener bind addresses. Defaults bind admin to loopback and the mTLS
/// listeners to all interfaces.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Listeners {
    pub members_addr: IpAddr,
    pub members_port: u16,
    pub clients_addr: IpAddr,
    pub clients_port: u16,
    pub admin_addr: IpAddr,
    pub admin_port: u16,
}

impl Default for Listeners {
    fn default() -> Self {
        Listeners {
            members_addr: "0.0.0.0".parse().unwrap(),
            members_port: 9551,
            clients_addr: "0.0.0.0".parse().unwrap(),
            clients_port: 9552,
            admin_addr: "127.0.0.1".parse().unwrap(),
            admin_port: 9553,
        }
    }
}

impl Listeners {
    pub fn members(&self) -> SocketAddr {
        SocketAddr::new(self.members_addr, self.members_port)
    }
    pub fn clients(&self) -> SocketAddr {
        SocketAddr::new(self.clients_addr, self.clients_port)
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

/// Quality classification thresholds (conservative defaults per handoff).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Quality {
    pub rtt_good_ms: f64,
    pub rtt_poor_ms: f64,
    pub rtt_bad_ms: f64,
    pub loss_good_pct: f64,
    pub loss_poor_pct: f64,
    pub rr_tps_good: f64,
    pub rr_tps_poor: f64,
    pub tcp_mbps_good: f64,
    pub tcp_mbps_poor: f64,
}

impl Default for Quality {
    fn default() -> Self {
        Quality {
            rtt_good_ms: 40.0,
            rtt_poor_ms: 90.0,
            rtt_bad_ms: 250.0,
            loss_good_pct: 1.0,
            loss_poor_pct: 5.0,
            rr_tps_good: 50.0,
            rr_tps_poor: 20.0,
            tcp_mbps_good: 10.0,
            tcp_mbps_poor: 2.0,
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
/// [cluster.members]
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
        assert_eq!(c.listeners.members_port, 9551);
        assert_eq!(c.listeners.clients_port, 9552);
        assert!(c.listeners.admin_addr.is_loopback());
        assert_eq!(c.listeners.admin_port, 9553);
    }
}
