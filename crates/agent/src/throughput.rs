//! Gated TCP throughput test: `iperf3 -J` against the peer's iperf3 server.
//!
//! Each side measures its own upload (client → server), which is the
//! direction its OSPF cost governs. The test is skipped (`busy`) while the
//! link already carries traffic above `quiet_max_mbps`, and only one test runs
//! at a time per node; pings pause around it (see `icmp::Gate`).

use crate::discover::Link;
use crate::icmp::Gate;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Outcome of a throughput request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// No measurement was taken: link in use, or another test running.
    pub busy: bool,
    /// Utilisation sampled at the gate (Mbps).
    pub util_mbps: f64,
    /// Measured upload; `None` when skipped or failed (never a fake 0).
    pub tcp_mbps: Option<f64>,
    /// Why the test produced no number (iperf3 failed), reported to the
    /// controller as the probe's note so a dead peer server is visible there.
    pub error: Option<String>,
}

pub struct Throughput {
    pub iperf_bin: String,
    pub port: u16,
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
                error: None,
            };
        }

        self.gate.begin();
        let res = self.iperf(link).await;
        self.gate.end(self.grace);

        match res {
            Ok(v) => Outcome {
                busy: false,
                util_mbps: util,
                tcp_mbps: Some(v),
                error: None,
            },
            Err(e) => {
                warn!("throughput {}: {e:#}", link.interface);
                // No number: the dimension stays unmeasured (never constrains
                // classification); the reason travels with the reply.
                Outcome {
                    busy: false,
                    util_mbps: util,
                    tcp_mbps: None,
                    error: Some(format!("{e:#}")),
                }
            }
        }
    }

    async fn iperf(&self, link: &Link) -> Result<f64> {
        let secs = self.duration.as_secs().max(1);
        let mut cmd = Command::new(&self.iperf_bin);
        cmd.args(["-J", "-6", "-c"])
            .arg(link.target.to_string())
            .args(["-B"])
            .arg(link.source.to_string())
            .args(["-p", &self.port.to_string(), "-t", &secs.to_string()])
            .kill_on_drop(true);

        let out = tokio::time::timeout(self.duration + Duration::from_secs(20), cmd.output())
            .await
            .map_err(|_| anyhow!("iperf3 overran its deadline"))?
            .with_context(|| format!("spawn {}", self.iperf_bin))?;

        let js: Value = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("iperf3 output ({})", out.status))?;
        parse_iperf(&js)
    }
}

/// Upload rate (Mbps) from `iperf3 -J` output: `end.sum_received` (what the
/// server actually got), else `end.sum`.
pub fn parse_iperf(js: &Value) -> Result<f64> {
    if let Some(err) = js["error"].as_str() {
        bail!("iperf3: {err}");
    }
    let end = &js["end"];
    let bps = end["sum_received"]["bits_per_second"]
        .as_f64()
        .or_else(|| end["sum"]["bits_per_second"].as_f64())
        .ok_or_else(|| anyhow!("iperf3: no end summary"))?;
    Ok(bps / 1e6)
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
        };
        let (l1, t1) = (link.clone(), t.clone());
        let first = tokio::spawn(async move { t1.run(&l1).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let second = t.run(&link).await;
        assert!(second.busy && second.tcp_mbps.is_none());
        let first = first.await.unwrap();
        assert!(!first.busy, "a failed test is not a busy link");
        assert!(first.tcp_mbps.is_none() && first.error.is_some());
        assert!(gate.paused(), "grace period after the test");
    }
}
