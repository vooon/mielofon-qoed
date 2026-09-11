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
/// constrain that dimension. `rtt_ms`/`loss_pct`/`jitter_ms` are upper bounds
/// (lower is better); `tcp_mbps` is a lower bound (higher is better).
/// `ospf_cost` is the metric advertised for links classified into this class.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QualityClass {
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    /// RTT jitter (ms), derived by the agent from the ping RTT distribution
    /// (iputils `mdev`, or the max-min spread on busybox). Upper bound.
    pub jitter_ms: Option<f64>,
    pub tcp_mbps: Option<f64>,
    pub ospf_cost: u32,
}

impl QualityClass {
    fn with(rtt_ms: f64, loss_pct: f64, jitter_ms: f64, tcp_mbps: f64, ospf_cost: u32) -> Self {
        QualityClass {
            rtt_ms: Some(rtt_ms),
            loss_pct: Some(loss_pct),
            jitter_ms: Some(jitter_ms),
            tcp_mbps: Some(tcp_mbps),
            ospf_cost,
        }
    }
}

/// Quality classification. A measurement is assigned the worst class whose
/// threshold it crosses; only dims listed per class take part. Conservative
/// defaults (per handoff): good/acceptable/poor/bad with increasing costs.
///
/// Jitter is the always-tier congestion signal: it is upper-bounded like rtt,
/// conservatively scaled below the rtt cutoffs (a healthy path has far lower
/// variation than delay). Bufferbloat/queueing inflate jitter before the
/// average RTT crosses a class, so jitter is a sensitive-but-stable latency
/// cross-check; strictness is tempered by the classifier's hysteresis.
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
            good: QualityClass::with(40.0, 1.0, 5.0, 10.0, 10),
            acceptable: QualityClass::with(90.0, 2.5, 15.0, 5.0, 20),
            poor: QualityClass::with(250.0, 5.0, 40.0, 2.0, 50),
            bad: QualityClass::with(500.0, 10.0, 100.0, 1.0, 100),
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

/// Console/logging configuration. `level` is the minimum `tracing` level
/// emitted to the console (default `info`), `format` selects the console
/// output format. Both are independent of the per-signal OTEL levels in
/// `[otel.*]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Log {
    /// Minimum console level: trace, debug, info, warn, error.
    pub level: String,
    /// Console log format: `logfmt` or `json`.
    pub format: String,
}

impl Default for Log {
    fn default() -> Self {
        Log {
            level: "info".into(),
            format: "logfmt".into(),
        }
    }
}

/// Frontend (dashboard) configuration.
///
/// `root_dir` is the directory containing the built Vue SPA (`index.html` +
/// `assets/`, including the bundled vis-network library). The controller
/// serves these files directly from disk (no embedding), so a package update
/// can ship a new bundle without rebuilding the daemon.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Frontend {
    pub root_dir: String,
}

impl Default for Frontend {
    fn default() -> Self {
        Frontend {
            root_dir: "/usr/share/mielofon/dashboard".into(),
        }
    }
}

/// Policy classifier configuration.
///
/// Classification is decoupled from measurement: every probe report is
/// appended to the replicated time-series store, and a background loop
/// (`classifier.rs`) derives each link's quality class / OSPF cost from a
/// window of recent samples rather than the latest single datapoint. These
/// knobs control that derivation.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Classifier {
    /// How often (seconds) the classifier recomputes per-link policy.
    pub interval_secs: u64,
    /// Freshness window (seconds) for the always-tier dimensions
    /// (rtt/loss/rr). Always probes run every ~15s, so 60s keeps them current.
    pub always_fresh_secs: u64,
    /// Freshness window (seconds) for the gated throughput dimension
    /// (tcp_mbps). Throughput probes run every ~300s and are skipped while a
    /// link is busy, so a longer window keeps mbps usable most of the time.
    pub tcp_fresh_secs: u64,
    /// Hard horizon (seconds) past which a dimension's last-known value is no
    /// longer carried: a tcp sample older than `tcp_carry_secs` does not
    /// constrain classification. Bounds how stale a carried value may be.
    pub tcp_carry_secs: u64,
    /// How long (seconds) a link whose most recent state is `busy`/`conflict`
    /// keeps its last derived cost instead of being re-classified. A busy link
    /// is never reported degraded while in use.
    pub busy_hold_secs: u64,
    /// Class-change hysteresis: a new quality class must be observed for this
    /// many consecutive classifier passes (`interval_secs` each) before it is
    /// applied. Prevents a single noisy sample — or a metric wobbling across a
    /// threshold — from flipping a link's class and re-routing OSPF cost. The
    /// first classification of a link applies immediately.
    pub hysteresis_passes: u32,
}

impl Default for Classifier {
    fn default() -> Self {
        Classifier {
            interval_secs: 5,
            always_fresh_secs: 60,
            tcp_fresh_secs: 1200,
            tcp_carry_secs: 3600,
            busy_hold_secs: 120,
            hysteresis_passes: 3,
        }
    }
}

/// Time-series history configuration.
///
/// `path` selects the durable embedded redb file; an empty path keeps history
/// in memory only (lost on restart). `raw_retention_secs` bounds the raw
/// sample ring, `agg_retention_secs` the 60s aggregate buckets.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TsConfig {
    pub path: String,
    pub raw_retention_secs: u64,
    pub agg_retention_secs: u64,
}

impl Default for TsConfig {
    fn default() -> Self {
        TsConfig {
            path: String::new(),
            raw_retention_secs: 7200,
            agg_retention_secs: 86400,
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
/// [log]
/// level = "info"
/// [ts]
/// path = "/etc/mielofon/tsdb.redb"
/// [frontend]
/// root_dir = "/usr/share/mielofon/dashboard"
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
    pub classifier: Classifier,
    pub log: Log,
    pub otel: mielofon_otel::OTelConfig,
    pub ts: TsConfig,
    pub frontend: Frontend,
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
