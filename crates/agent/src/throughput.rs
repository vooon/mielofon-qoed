//! Gated throughput test: `iperf3 -J` against the peer's iperf3 server, a
//! TCP upload and then a fixed-rate UDP stream in the same gate window.
//!
//! TCP gives the usable bulk rate; the UDP leg sends at `udp_rate_mbps` and
//! reports what the server received, its jitter and its packet loss — the
//! raw path, free of TCP's congestion control, which is what shows shaping
//! and drops directly. Each side measures its own upload (client → server),
//! which is the direction its OSPF cost governs. Like the pings, the test targets the OSPF
//! neighbour's link-local address with the interface scope when it is known,
//! so the stream runs over this tunnel rather than the route OSPF prefers to
//! the peer's global address. The test is skipped (`busy`) while the link
//! already carries traffic above `quiet_max_mbps`, and only one test runs at
//! a time per node; pings pause around it (see `icmp::Gate`).

use crate::discover::Link;
use crate::icmp::Gate;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, warn, Instrument};

/// Outcome of a throughput request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// No measurement was taken: link in use, or another test running.
    pub busy: bool,
    /// Utilisation sampled at the gate (Mbps).
    pub util_mbps: f64,
    /// Measured upload; `None` when skipped or failed (never a fake 0).
    pub tcp_mbps: Option<f64>,
    /// The UDP leg's result; `None` when disabled, skipped or failed.
    pub udp: Option<UdpResult>,
    /// Why a leg produced no number (iperf3 failed), reported to the
    /// controller as the probe's note so a dead peer server is visible there.
    pub error: Option<String>,
}

/// What the peer's server received from the fixed-rate UDP stream.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UdpResult {
    pub mbps: f64,
    pub jitter_ms: Option<f64>,
    pub loss_pct: Option<f64>,
}

pub struct Throughput {
    pub iperf_bin: String,
    pub port: u16,
    /// UDP leg port (`port` when 0) and send rate (0 = no UDP leg).
    pub udp_port: u16,
    pub udp_rate_mbps: f64,
    pub duration: Duration,
    pub quiet_max_mbps: f64,
    /// Pings stay paused this long after a test.
    pub grace: Duration,
    pub gate: Arc<Gate>,
    lock: Mutex<()>,
}

impl Throughput {
    pub fn new(
        iperf_bin: String,
        port: u16,
        duration: Duration,
        quiet_max_mbps: f64,
        grace: Duration,
        gate: Arc<Gate>,
    ) -> Self {
        Throughput {
            iperf_bin,
            port,
            udp_port: 0,
            udp_rate_mbps: 0.0,
            duration,
            quiet_max_mbps,
            grace,
            gate,
            lock: Mutex::new(()),
        }
    }

    pub async fn run(&self, link: &Link) -> Outcome {
        let Ok(_guard) = self.lock.try_lock() else {
            debug!("throughput {}: another test running", link.interface);
            return Outcome {
                busy: true,
                ..Outcome::default()
            };
        };

        let util = match util_mbps(&link.interface, Duration::from_secs(1)).await {
            Ok(u) => u,
            Err(e) => {
                warn!("throughput {}: util sample: {e:#}", link.interface);
                0.0
            }
        };
        if util > self.quiet_max_mbps {
            return Outcome {
                busy: true,
                util_mbps: util,
                tcp_mbps: None,
                udp: None,
                error: None,
            };
        }

        self.gate.begin();
        let tcp = self.iperf(link, Leg::Tcp).await;
        let udp = if self.udp_rate_mbps > 0.0 {
            Some(self.iperf(link, Leg::Udp).await)
        } else {
            None
        };
        self.gate.end(self.grace);

        // A leg without a number stays unmeasured (never constrains
        // classification); the reason travels with the reply.
        let mut errors = Vec::new();
        let tcp_mbps = match tcp.and_then(|js| parse_iperf(&js)) {
            Ok(v) => Some(v),
            Err(e) => {
                errors.push(format!("tcp: {e:#}"));
                None
            }
        };
        let udp = udp.and_then(|r| match r.and_then(|js| parse_iperf_udp(&js)) {
            Ok(u) => Some(u),
            Err(e) => {
                errors.push(format!("udp: {e:#}"));
                None
            }
        });
        if !errors.is_empty() {
            warn!("throughput {}: {}", link.interface, errors.join("; "));
        }
        Outcome {
            busy: false,
            util_mbps: util,
            tcp_mbps,
            udp,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    /// One `iperf3 -J` client run; the parsed JSON document.
    async fn iperf(&self, link: &Link, leg: Leg) -> Result<Value> {
        let span = tracing::info_span!("iperf3", leg = ?leg, iface = %link.interface);
        self.iperf_run(link, leg).instrument(span).await
    }

    async fn iperf_run(&self, link: &Link, leg: Leg) -> Result<Value> {
        let secs = self.duration.as_secs().max(1);
        let mut cmd = Command::new(&self.iperf_bin);
        cmd.args(["-J", "-6", "-c"]);
        match link.peer_ll {
            // Scoped link-local: the kernel sources it from this interface.
            Some(ll) => {
                cmd.arg(format!("{ll}%{}", link.interface));
            }
            None => {
                cmd.arg(link.target.to_string())
                    .arg("-B")
                    .arg(link.source.to_string());
            }
        }
        let port = match leg {
            Leg::Tcp => self.port,
            Leg::Udp if self.udp_port > 0 => self.udp_port,
            Leg::Udp => self.port,
        };
        cmd.args(["-p", &port.to_string(), "-t", &secs.to_string()]);
        if leg == Leg::Udp {
            cmd.arg("-u")
                .arg("-b")
                .arg(format!("{}M", self.udp_rate_mbps));
        }
        cmd.kill_on_drop(true);

        let out = tokio::time::timeout(self.duration + Duration::from_secs(20), cmd.output())
            .await
            .map_err(|_| anyhow!("iperf3 overran its deadline"))?
            .with_context(|| format!("spawn {}", self.iperf_bin))?;

        serde_json::from_slice(&out.stdout)
            .with_context(|| format!("iperf3 output ({})", out.status))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leg {
    Tcp,
    Udp,
}

/// The `end` summary of an `iperf3 -J` run: `sum_received` (what the server
/// actually got), else `sum`.
fn end_summary(js: &Value) -> Result<&Value> {
    if let Some(err) = js["error"].as_str() {
        bail!("iperf3: {err}");
    }
    let end = &js["end"];
    for key in ["sum_received", "sum"] {
        if end[key]["bits_per_second"].is_number() {
            return Ok(&end[key]);
        }
    }
    bail!("iperf3: no end summary")
}

/// Upload rate (Mbps) of a TCP run.
pub fn parse_iperf(js: &Value) -> Result<f64> {
    Ok(end_summary(js)?["bits_per_second"].as_f64().unwrap_or(0.0) / 1e6)
}

/// Received rate, jitter and loss of a UDP run. iperf3 reports the server's
/// view of a UDP stream under `end.sum` (older) or `end.sum_received`.
pub fn parse_iperf_udp(js: &Value) -> Result<UdpResult> {
    let sum = end_summary(js)?;
    Ok(UdpResult {
        mbps: sum["bits_per_second"].as_f64().unwrap_or(0.0) / 1e6,
        jitter_ms: sum["jitter_ms"].as_f64(),
        loss_pct: sum["lost_percent"].as_f64(),
    })
}

/// rx+tx byte counter of an interface.
pub fn read_bytes(iface: &str) -> Result<u64> {
    let mut total = 0;
    for dir in ["rx_bytes", "tx_bytes"] {
        let p = format!("/sys/class/net/{iface}/statistics/{dir}");
        let s = std::fs::read_to_string(&p).with_context(|| p.clone())?;
        total += s.trim().parse::<u64>().with_context(|| p.clone())?;
    }
    Ok(total)
}

/// Interface utilisation (rx+tx, Mbps) over `span`.
pub async fn util_mbps(iface: &str, span: Duration) -> Result<f64> {
    let a = read_bytes(iface)?;
    let t = tokio::time::Instant::now();
    tokio::time::sleep(span).await;
    let b = read_bytes(iface)?;
    Ok(rate_mbps(a, b, t.elapsed()))
}

pub fn rate_mbps(before: u64, after: u64, span: Duration) -> f64 {
    let secs = span.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    after.saturating_sub(before) as f64 * 8.0 / 1e6 / secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_iperf_json() {
        let tcp = json!({ "end": {
            "sum_sent": { "bits_per_second": 160e6 },
            "sum_received": { "bits_per_second": 150e6 }
        }});
        assert!((parse_iperf(&tcp).unwrap() - 150.0).abs() < 1e-9);

        let sum_only = json!({ "end": { "sum": { "bits_per_second": 50e6 } } });
        assert!((parse_iperf(&sum_only).unwrap() - 50.0).abs() < 1e-9);

        let refused = json!({ "start": {}, "end": {},
            "error": "unable to connect to server: Connection refused" });
        assert!(parse_iperf(&refused)
            .unwrap_err()
            .to_string()
            .contains("refused"));

        assert!(parse_iperf(&json!({})).is_err());
    }

    #[test]
    fn parses_udp_summary() {
        let udp = json!({ "end": { "sum": {
            "bits_per_second": 19.75e6, "jitter_ms": 1.25,
            "lost_packets": 10, "packets": 2000, "lost_percent": 0.5
        }}});
        let r = parse_iperf_udp(&udp).unwrap();
        assert!((r.mbps - 19.75).abs() < 1e-9);
        assert_eq!((r.jitter_ms, r.loss_pct), (Some(1.25), Some(0.5)));
        // A server-side summary wins when present.
        let both = json!({ "end": {
            "sum": { "bits_per_second": 20e6, "lost_percent": 0.0 },
            "sum_received": { "bits_per_second": 18e6, "lost_percent": 10.0, "jitter_ms": 2.0 }
        }});
        let r = parse_iperf_udp(&both).unwrap();
        assert!((r.mbps - 18.0).abs() < 1e-9);
        assert_eq!(r.loss_pct, Some(10.0));
    }

    #[test]
    fn rate_is_bits_per_second() {
        assert!((rate_mbps(0, 1_250_000, Duration::from_secs(1)) - 10.0).abs() < 1e-9);
        assert_eq!(
            rate_mbps(10, 5, Duration::from_secs(1)),
            0.0,
            "counter reset"
        );
        assert_eq!(rate_mbps(0, 5, Duration::ZERO), 0.0);
    }

    #[tokio::test]
    async fn second_concurrent_test_is_busy() {
        let gate = Arc::new(Gate::default());
        // The first run holds the lock through its 1 s utilisation sample;
        // `false` then stands in for a failing iperf3.
        let t = Arc::new(Throughput::new(
            "false".into(),
            5201,
            Duration::from_secs(1),
            1e9,
            Duration::from_secs(5),
            gate.clone(),
        ));
        let link = Link {
            interface: "lo".into(),
            to: "self".into(),
            source: "::1".parse().unwrap(),
            target: "::1".parse().unwrap(),
            peer_ll: None,
            cost: None,
        };
        let (l1, t1) = (link.clone(), t.clone());
        let first = tokio::spawn(async move { t1.run(&l1).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let second = t.run(&link).await;
        assert!(second.busy && second.tcp_mbps.is_none());
        let first = first.await.unwrap();
        assert!(!first.busy, "a failed test is not a busy link");
        assert!(first.tcp_mbps.is_none() && first.udp.is_none() && first.error.is_some());
        assert!(gate.paused(), "grace period after the test");
    }
}
