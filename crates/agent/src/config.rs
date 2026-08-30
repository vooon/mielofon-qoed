//! Agent configuration (TOML). All values are device-local and sanitized.

use mielofon_otel::OTelConfig;
use serde::Deserialize;

/// One managed outgoing link. The probe target/source are device-local
/// addresses; the controller only ever references the link by its key.
#[derive(Debug, Clone, Deserialize)]
pub struct LinkConfig {
    pub from: String,
    pub to: String,
    pub interface: String,
    /// Probe destination (e.g. the peer's loopback reachable only via this
    /// link). Placeholder address in examples.
    pub target: String,
    /// Optional source address to bind probes to this specific interface.
    #[serde(default)]
    pub source: Option<String>,
    /// Optional shell command applied on apply_cost. `{interface}` and
    /// `{cost}` placeholders are substituted. Empty = no-op.
    #[serde(default)]
    pub cost_command: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Loop {
    pub poll_secs: u64,
}

impl Default for Loop {
    fn default() -> Self {
        Loop { poll_secs: 2 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Probes {
    /// Ping count for the always-on RTT/loss probe.
    pub ping_count: u32,
    /// ping interval in seconds (iputils `-i`).
    pub ping_interval: f64,
    /// netperf TCP_RR duration in seconds.
    pub rr_duration: u8,
    /// iperf3 throughput duration in seconds.
    pub tcp_duration: u8,
    /// Quiet threshold in Mbps: above this the throughput tier is skipped.
    pub quiet_max_mbps: f64,
}

impl Default for Probes {
    fn default() -> Self {
        Probes {
            ping_count: 3,
            ping_interval: 0.2,
            rr_duration: 4,
            tcp_duration: 4,
            quiet_max_mbps: 15.0,
        }
    }
}

/// Top-level agent configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub agent: String,
    /// Controller clients listener address (mTLS).
    pub controller: String,
    pub controller_port: u16,
    pub tls: TlsConfig,
    pub links: Vec<LinkConfig>,
    pub loop_: Loop,
    pub probes: Probes,
    pub otel: OTelConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agent: "spoke-1".into(),
            controller: "203.0.113.1".into(),
            controller_port: 9552,
            tls: TlsConfig::default(),
            links: Vec::new(),
            loop_: Loop::default(),
            probes: Probes::default(),
            otel: OTelConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub ca: String,
    pub cert: String,
    pub key: String,
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig {
            ca: "/etc/mielofon/ca.pem".into(),
            cert: "/etc/mielofon/agent.pem".into(),
            key: "/etc/mielofon/agent.key".into(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read config {}: {}", path, e))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse config {}: {}", path, e))
    }

    /// Link config by interface (probe commands carry only the link key).
    pub fn link_by_interface(&self, interface: &str) -> Option<&LinkConfig> {
        self.links.iter().find(|l| l.interface == interface)
    }
}
