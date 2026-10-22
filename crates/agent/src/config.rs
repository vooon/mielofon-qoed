//! Agent configuration from UCI (`/etc/config/mielofon-agent`).
//!
//! Read through `ubus call uci get` so the agent needs no UCI parser of its
//! own. UCI values are strings; numeric options fall back to their defaults
//! when absent or malformed.

use crate::bus::Bus;
use anyhow::{bail, Context, Result};
use mielofon_transport::{parse_endpoint, Endpoint};
use serde_json::{json, Map, Value};
use std::time::Duration;

const PACKAGE: &str = "mielofon-agent";
/// The controller's client listener port when an endpoint names none.
const DEFAULT_PORT: u16 = 9552;

#[derive(Debug, Clone)]
pub struct Config {
    /// Node identity (`from` of every link); defaults to the hostname.
    pub agent: String,
    /// Controller endpoints (`host:port`, names or literal addresses), tried
    /// in order; a name resolving to several addresses yields all of them.
    pub endpoints: Vec<Endpoint>,
    pub cacert: String,
    pub cert: String,
    pub key: String,
    /// OSPF protocol whose ptp interfaces are the managed links.
    pub ospf_protocol: String,
    /// Tunnel interface prefix (`awg_hub1` minus `awg_` = `hub1`).
    /// Shell globs (`*`, `?`) selecting the managed tunnel interfaces among
    /// the mesh OSPF protocol's point-to-point ones: `awg_*` on a spoke,
    /// `awg_int_*` plus `awg_sp_*` on a hub.
    pub ifaces: Vec<String>,
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
    /// Link re-discovery cadence (`bird status` + netifd over ubus, and the
    /// applied-cost drift check that rides on the same snapshot).
    pub refresh_interval: Duration,
    /// Syslog-style level: debug, info, notice, warning, err.
    pub log_level: String,
    /// OTLP/HTTP collector (`http(s)://host:port`); empty disables OTEL.
    pub otel_endpoint: String,
    /// Export spans (one per command, with its iperf3 legs) to the collector.
    pub otel_traces: bool,
    /// Also forward the log to the collector (syslog usually has it already).
    pub otel_logs: bool,
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

/// A UCI boolean: `1`/`0`, `on`/`off`, `true`/`false`, `yes`/`no`.
fn flag(sec: &Map<String, Value>, name: &str, def: bool) -> bool {
    match opt(sec, name).map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "on" | "true" | "yes" | "enabled") => true,
        Some("0" | "off" | "false" | "no" | "disabled") => false,
        _ => def,
    }
}

/// A UCI `list` option: an array in `ubus call uci get`, or a single string
/// when the option was written as `option`.
fn list<'a>(sec: &'a Map<String, Value>, name: &str) -> Vec<&'a str> {
    match sec.get(name) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => vec![s.as_str()],
        _ => Vec::new(),
    }
}

/// `list iface` globs (or one `option`, space-separated). The pre-glob
/// `iface_prefix` is honoured as `<prefix>*` when no glob is set. Empty
/// config keeps the historical `awg_*`.
fn iface_globs(main: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = list(main, "iface")
        .iter()
        .flat_map(|e| e.split_whitespace())
        .map(str::to_string)
        .collect();
    if out.is_empty() {
        out = list(main, "iface_prefix")
            .iter()
            .flat_map(|e| e.split_whitespace())
            .map(|p| format!("{p}*"))
            .collect();
    }
    out.sort();
    out.dedup();
    if out.is_empty() {
        out.push("awg_*".to_string());
    }
    out
}

/// `list endpoint`, or the pre-QUIC `controller_url` with its scheme and
/// path stripped, so an upgraded node keeps talking to the same hub.
fn endpoints(main: &Map<String, Value>) -> Result<Vec<Endpoint>> {
    let mut entries: Vec<String> = list(main, "endpoint")
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if entries.is_empty() {
        if let Some(url) = opt(main, "controller_url") {
            let host = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or_default();
            entries.push(host.to_string());
        }
    }
    if entries.is_empty() {
        bail!("{PACKAGE}.main.endpoint is not set");
    }
    entries
        .iter()
        .map(|e| parse_endpoint(e, DEFAULT_PORT).map_err(|err| anyhow::anyhow!("endpoint {err}")))
        .collect()
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
            endpoints: endpoints(main)?,
            cacert: required("cacert")?,
            cert: required("cert")?,
            key: required("key")?,
            ospf_protocol: opt(main, "ospf_protocol").unwrap_or("mesh_v3").to_string(),
            ifaces: iface_globs(main),
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
            refresh_interval: Duration::from_secs(num(main, "refresh_interval", 60_u64).max(10)),
            log_level: opt(main, "log_level").unwrap_or("notice").to_string(),
            otel_endpoint: opt(main, "otel_endpoint").unwrap_or("").to_string(),
            otel_traces: flag(main, "otel_traces", true),
            otel_logs: flag(main, "otel_logs", false),
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
                "endpoint": ["hub.example.com:9552", "[2001:db8::1]:9552", "203.0.113.1"],
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
        let eps: Vec<(String, u16)> = c
            .endpoints
            .iter()
            .map(|e| (e.host.clone(), e.port))
            .collect();
        assert_eq!(
            eps,
            vec![
                ("hub.example.com".to_string(), 9552),
                ("2001:db8::1".to_string(), 9552),
                ("203.0.113.1".to_string(), 9552),
            ]
        );
        assert_eq!(c.ping_interval, Duration::from_millis(500));
        assert_eq!(c.refresh_interval, Duration::from_secs(60), "default");
        assert_eq!(c.quiet_max_mbps, 15.0, "malformed number falls back");
        assert_eq!(c.tcp_duration, Duration::from_secs(6));
        assert_eq!(c.ospf_protocol, "mesh_v3");
        assert_eq!(c.ifaces, vec!["awg_*".to_string()], "default glob");
        assert_eq!(c.excludes, vec!["awg_legacy".to_string()]);
        assert!(c.prometheus_textfile.is_none());
        assert_eq!((c.udp_port, c.udp_rate_mbps), (0, 20.0));
    }

    #[test]
    fn legacy_controller_url_becomes_the_endpoint() {
        let mut v = values();
        let main = v["main"].as_object_mut().unwrap();
        main.remove("endpoint");
        main.insert("controller_url".into(), json!("https://203.0.113.1:9552/"));
        let c = Config::from_uci(&v, "node-0").unwrap();
        assert_eq!(c.endpoints.len(), 1);
        assert_eq!(c.endpoints[0].host, "203.0.113.1");
        assert_eq!(c.endpoints[0].port, 9552);

        v["main"].as_object_mut().unwrap().remove("controller_url");
        let err = Config::from_uci(&v, "node-0").unwrap_err();
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn iface_globs_come_as_a_list_or_one_option_or_the_legacy_prefix() {
        let mut v = values();
        v["main"]["iface"] = json!(["awg_sp_*", "awg_int_*"]);
        let c = Config::from_uci(&v, "node-0").unwrap();
        assert_eq!(
            c.ifaces,
            vec!["awg_int_*".to_string(), "awg_sp_*".to_string()]
        );

        v["main"]["iface"] = json!("awg_int_* awg_sp_* awg_sp_*");
        let c = Config::from_uci(&v, "node-0").unwrap();
        assert_eq!(
            c.ifaces,
            vec!["awg_int_*".to_string(), "awg_sp_*".to_string()]
        );

        v["main"].as_object_mut().unwrap().remove("iface");
        v["main"]["iface_prefix"] = json!("awg_int_");
        let c = Config::from_uci(&v, "node-0").unwrap();
        assert_eq!(c.ifaces, vec!["awg_int_*".to_string()], "legacy prefix");
    }

    #[test]
    fn otel_is_off_without_an_endpoint_and_flags_parse() {
        let c = Config::from_uci(&values(), "node-0").unwrap();
        assert_eq!(c.otel_endpoint, "");
        assert!(c.otel_traces && !c.otel_logs, "defaults");
        let mut v = values();
        v["main"]["otel_endpoint"] = json!("https://collector.example:4318");
        v["main"]["otel_logs"] = json!("on");
        v["main"]["otel_traces"] = json!("0");
        let c = Config::from_uci(&v, "node-0").unwrap();
        assert_eq!(c.otel_endpoint, "https://collector.example:4318");
        assert!(!c.otel_traces && c.otel_logs);
    }

    #[test]
    fn missing_identity_is_an_error() {
        let mut v = values();
        v["main"].as_object_mut().unwrap().remove("cert");
        let err = Config::from_uci(&v, "node-0").unwrap_err();
        assert!(err.to_string().contains("cert"));
    }
}
