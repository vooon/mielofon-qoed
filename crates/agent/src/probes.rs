//! Probe execution. The agent only runs the probes it is told to run and
//! returns raw measurements; it makes no scheduling or policy decisions.

use crate::config::{LinkConfig, Probes};
use crate::model::ProbeState;
use anyhow::Context;
use std::process::Stdio;
use tokio::process::Command;

/// Result of an always-on probe run.
pub struct AlwaysProbe {
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub rr_tps: f64,
}

/// Result of a gated throughput probe run. `tcp_mbps` is `None` when a link
/// was busy and the throughput probe was skipped.
pub struct ThroughputProbe {
    pub tcp_mbps: Option<f64>,
    pub util_mbps: f64,
}

/// Run the always-on tier: RTT + loss (ping) and transaction rate (netperf
/// TCP_RR). Never gated.
pub async fn run_always(cfg: &Probes, l: &LinkConfig) -> anyhow::Result<AlwaysProbe> {
    let (rtt_ms, loss_pct) = ping_measure(cfg, l).await.unwrap_or((f64::NAN, f64::NAN));
    let rr_tps = netperf_rr(cfg, l).await.unwrap_or(f64::NAN);
    Ok(AlwaysProbe {
        rtt_ms,
        loss_pct,
        rr_tps,
    })
}

/// Run the gated throughput tier: sample link utilization, and only when quiet
/// run iperf3 TCP throughput. Busy links report `busy` and skip the probe.
pub async fn run_throughput(cfg: &Probes, l: &LinkConfig) -> anyhow::Result<ThroughputProbe> {
    let util = util_mbps(&l.interface).await.unwrap_or(0.0);
    if util > cfg.quiet_max_mbps {
        return Ok(ThroughputProbe {
            tcp_mbps: None,
            util_mbps: util,
        });
    }
    let tcp_mbps = iperf3_tcp(cfg, l).await.ok();
    Ok(ThroughputProbe {
        tcp_mbps,
        util_mbps: util,
    })
}

/// Whether a throughput probe's skipping happened due to the quiet gate.
pub fn is_busy(tp: &ThroughputProbe) -> bool {
    tp.tcp_mbps.is_none()
}

pub fn state_for(tp: &ThroughputProbe) -> ProbeState {
    if is_busy(tp) {
        ProbeState::Busy
    } else {
        ProbeState::Quiet
    }
}

async fn ping_measure(cfg: &Probes, l: &LinkConfig) -> anyhow::Result<(f64, f64)> {
    let mut cmd = Command::new("ping");
    cmd.arg("-q")
        .arg("-c")
        .arg(cfg.ping_count.to_string())
        .arg("-i")
        .arg(cfg.ping_interval.to_string())
        .arg("-W")
        .arg("1");
    if let Some(src) = &l.source {
        cmd.arg("-I").arg(src);
    }
    cmd.arg(&l.target)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = cmd.output().await.context("run ping")?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();

    let (loss, rtt) = parse_ping(&text);
    Ok((rtt, loss))
}

fn parse_ping(text: &str) -> (f64, f64) {
    let loss = text
        .lines()
        .find(|l| l.contains("packet loss"))
        .and_then(|l| l.split(',').nth(2))
        .and_then(|p| p.trim().trim_end_matches('%').parse::<f64>().ok())
        .unwrap_or(-1.0);

    let rtt = text
        .lines()
        .find(|l| l.contains("rtt ") || l.contains("round-trip"))
        .and_then(|l| l.split('=').nth(1))
        .and_then(|rest| rest.split('/').next())
        .and_then(|min| min.trim().parse::<f64>().ok())
        .unwrap_or(f64::NAN);

    (loss, rtt)
}

async fn netperf_rr(cfg: &Probes, l: &LinkConfig) -> anyhow::Result<f64> {
    let mut cmd = Command::new("netperf");
    cmd.arg("-l")
        .arg(cfg.rr_duration.to_string())
        .arg("-t")
        .arg("TCP_RR")
        .arg("-H")
        .arg(&l.target);
    if let Some(src) = &l.source {
        cmd.arg("-L").arg(src);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let out = cmd.output().await.context("run netperf")?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_transaction_rate(&text)
}

fn parse_transaction_rate(text: &str) -> anyhow::Result<f64> {
    // netperf TCP_RR table: rows of numbers; the last column is trans/sec.
    let rates: Vec<f64> = text
        .lines()
        .filter(|l| l.trim().split_whitespace().count() >= 5)
        .filter_map(|l| l.trim().split_whitespace().last())
        .filter_map(|w| w.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .collect();
    rates
        .last()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("no transaction rate in netperf output"))
}

async fn iperf3_tcp(cfg: &Probes, l: &LinkConfig) -> anyhow::Result<f64> {
    let mut cmd = Command::new("iperf3");
    cmd.arg("-c")
        .arg(&l.target)
        .arg("-t")
        .arg(cfg.tcp_duration.to_string())
        .arg("-f")
        .arg("m")
        .arg("-J");
    if let Some(src) = &l.source {
        cmd.arg("-B").arg(src);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let out = cmd.output().await.context("run iperf3")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim()).context("parse iperf3 json")?;
    let bits = v["end"]["sum_received"]["bits_per_second"]
        .as_f64()
        .or_else(|| v["end"]["sum"]["bits_per_second"].as_f64())
        .ok_or_else(|| anyhow::anyhow!("no sum in iperf3 json"))?;
    Ok(bits / 1_000_000.0)
}

/// Sample instantaneous link utilization from `/sys/class/net/<iface>/statistics`.
async fn util_mbps(iface: &str) -> anyhow::Result<f64> {
    async fn bytes(iface: &str) -> anyhow::Result<u64> {
        let rx: u64 =
            tokio::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/rx_bytes"))
                .await?
                .trim()
                .parse()?;
        let tx: u64 =
            tokio::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/tx_bytes"))
                .await?
                .trim()
                .parse()?;
        Ok(rx + tx)
    }
    let a = bytes(iface).await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let b = bytes(iface).await?;
    let mbps = (b.saturating_sub(a)) as f64 * 8.0 / 1_000_000.0;
    Ok(mbps)
}
