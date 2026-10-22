//! Agent configuration from UCI (`/etc/config/mielofon-agent`).
//!
//! Read through `ubus call uci get` so the agent needs no UCI parser of its
//! own. UCI values are strings; numeric options fall back to their defaults
//! when absent or malformed.

use crate::bus::Bus;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

const PACKAGE: &str = "mielofon-agent";

#[derive(Debug, Clone)]
pub struct Config {
    /// Node identity (`from` of every link); defaults to the hostname.
    pub agent: String,
    /// Controller clients listener, e.g. `https://203.0.113.1:9552`.
    pub controller_url: String,
    pub cacert: String,
    pub cert: String,
    pub key: String,
    /// Long-poll hold for `/v1/agent/command`.
    pub command_timeout: Duration,
    /// OSPF protocol whose ptp interfaces are the managed links.
    pub ospf_protocol: String,
    /// Tunnel interface prefix (`awg_hub1` minus `awg_` = `hub1`).
    pub iface_prefix: String,
    /// Interface carrying the mesh loopback reported at register.
    pub loopback_iface: String,
    /// Discovered interfaces never to manage.
    pub excludes: Vec<String>,
    /// Ping cadence per link.
    pub ping_interval: Duration,
    /// Rolling window of pings the rtt/jitter/loss statistics are taken over.
    pub ping_window: usize,
    /// Above this utilisation (Mbps) the throughput test is skipped (busy).
    pub quiet_max_mbps: f64,
    /// Duration of the TCP throughput test.
    pub tcp_duration: Duration,
    /// Peer iperf3 server port (TCP leg).
    pub iperf_port: u16,
    /// Peer iperf3 port for the UDP leg; `iperf_port` when unset.
    pub udp_port: u16,
    /// Send rate of the UDP leg (Mbps); 0 disables the leg.
    pub udp_rate_mbps: f64,
    /// iperf3 client binary.
    pub iperf_bin: String,
    /// Prometheus textfile output (empty disables).
    pub prometheus_textfile: Option<String>,
    pub prometheus_interval: Duration,
    /// Syslog-style level: debug, info, notice, warning, err.
    pub log_level: String,
}

fn opt<'a>(sec: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    sec.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn num<T: std::str::FromStr>(sec: &Map<String, Value>, name: &str, def: T) -> T {
    opt(sec, name).and_then(|v| v.parse().ok()).unwrap_or(def)
}

impl Config {
    /// Build from the `values` object of `ubus call uci get`.
    pub fn from_uci(values: &Value, hostname: &str) -> Result<Config> {
        let empty = Map::new();
        let sections = values.as_object().unwrap_or(&empty);
        let main = sections
            .get("main")
            .and_then(Value::as_object)
            .unwrap_or(&empty);

        let required = |name: &str| -> Result<String> {
            opt(main, name)
                .map(str::to_string)
                .with_context(|| format!("{PACKAGE}.main.{name} is not set"))
        };

        let mut excludes: Vec<String> = sections
            .values()
            .filter_map(Value::as_object)
            .filter(|s| s.get(".type").and_then(Value::as_str) == Some("exclude"))
            .filter_map(|s| opt(s, "interface").map(str::to_string))
            .collect();
        excludes.sort();

        let ping_interval = num(main, "ping_interval", 1.0_f64).clamp(0.1, 60.0);
        let textfile = opt(main, "prometheus_textfile").map(str::to_string);

        Ok(Config {
            agent: opt(main, "agent_name").unwrap_or(hostname).to_string(),
            controller_url: required("controller_url")?
                .trim_end_matches('/')
                .to_string(),
            cacert: required("cacert")?,
            cert: required("cert")?,
            key: required("key")?,
            command_timeout: Duration::from_millis(num(main, "command_timeout_ms", 30_000)),
            ospf_protocol: opt(main, "ospf_protocol").unwrap_or("mesh_v3").to_string(),
            iface_prefix: opt(main, "iface_prefix").unwrap_or("awg_").to_string(),
            loopback_iface: opt(main, "loopback_iface")
                .unwrap_or("dummy_awg")
                .to_string(),
            excludes,
            ping_interval: Duration::from_secs_f64(ping_interval),
            ping_window: num(main, "ping_window", 60_usize).clamp(2, 3600),
            quiet_max_mbps: num(main, "quiet_max_mbps", 15.0),
            tcp_duration: Duration::from_secs(num(main, "tcp_duration", 4_u64).clamp(1, 60)),
            iperf_port: num(main, "iperf_port", 5201),
            udp_port: num(main, "udp_port", 0),
            udp_rate_mbps: num(main, "udp_rate_mbps", 20.0_f64).max(0.0),
            iperf_bin: opt(main, "iperf_bin").unwrap_or("iperf3").to_string(),
            prometheus_textfile: textfile,
            prometheus_interval: Duration::from_secs(
                num(main, "prometheus_interval", 20_u64).max(1),
            ),
            log_level: opt(main, "log_level").unwrap_or("notice").to_string(),
        })
    }

    /// Load the live UCI config over ubus.
    pub async fn load(bus: &impl Bus) -> Result<Config> {
        let reply = bus
            .call("uci", "get", json!({ "config": PACKAGE }))
            .await
            .context("read UCI config")?;
        let Some(values) = reply.get("values") else {
            bail!("uci get {PACKAGE}: no values");
        };
        Config::from_uci(values, &hostname())
    }
}

/// The router hostname, the default agent identity.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| PACKAGE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Value {
        json!({
            "main": {
                ".type": "agent", ".name": "main",
                "controller_url": "https://203.0.113.1:9552/",
                "cacert": "/etc/mielofon/ca.pem",
                "cert": "/etc/mielofon/agent.cer",
                "key": "/etc/mielofon/agent.key",
                "ping_interval": "0.5",
                "quiet_max_mbps": "not-a-number",
                "tcp_duration": "6"
            },
            "cfg0a1b2c": { ".type": "exclude", "interface": "awg_legacy" },
            "cfg0d3e4f": { ".type": "exclude" }
        })
    }

    #[test]
    fn parses_uci_with_defaults() {
        let c = Config::from_uci(&values(), "node-0").unwrap();
        assert_eq!(c.agent, "node-0", "hostname is the default identity");
        assert_eq!(c.controller_url, "https://203.0.113.1:9552");
        assert_eq!(c.ping_interval, Duration::from_millis(500));
        assert_eq!(c.quiet_max_mbps, 15.0, "malformed number falls back");
        assert_eq!(c.tcp_duration, Duration::from_secs(6));
        assert_eq!(c.ospf_protocol, "mesh_v3");
        assert_eq!(c.excludes, vec!["awg_legacy".to_string()]);
        assert!(c.prometheus_textfile.is_none());
        assert_eq!((c.udp_port, c.udp_rate_mbps), (0, 20.0));
    }

    #[test]
    fn missing_identity_is_an_error() {
        let mut v = values();
        v["main"].as_object_mut().unwrap().remove("cert");
        let err = Config::from_uci(&v, "node-0").unwrap_err();
        assert!(err.to_string().contains("cert"));
    }
}
