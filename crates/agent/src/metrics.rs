//! Prometheus self-observability via the node-exporter textfile collector.
//!
//! Metric names match the former ucode agent (`mielofon_agent_*`) so existing
//! dashboards keep working; the ping counters of the former C++ probe
//! daemon are folded in as `mielofon_agent_pings_*_total`.

use crate::icmp::LinkStats;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Default)]
pub struct Counters {
    pub commands_received: AtomicU64,
    pub commands_succeeded: AtomicU64,
    pub commands_errored: AtomicU64,
    pub probe_ping: AtomicU64,
    pub probe_iperf: AtomicU64,
    pub probe_busy: AtomicU64,
    pub probe_errors_ping: AtomicU64,
    pub probe_errors_iperf: AtomicU64,
    pub apply_cost: AtomicU64,
    pub apply_cost_errors: AtomicU64,
}

impl Counters {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// Last throughput result per link.
#[derive(Debug, Clone, Default)]
pub struct LastThroughput {
    pub tcp_mbps: Option<f64>,
    pub util_mbps: f64,
    pub busy: bool,
    pub unixtime: u64,
}

/// Link label set: (from, to, interface).
pub type Labels = (String, String, String);

#[derive(Default)]
pub struct Metrics {
    pub counters: Counters,
    pub throughput: Mutex<BTreeMap<String, LastThroughput>>,
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn lbl(l: &Labels) -> String {
    format!(
        "{{from=\"{}\",to=\"{}\",interface=\"{}\"}}",
        esc(&l.0),
        esc(&l.1),
        esc(&l.2)
    )
}

struct Out(String);

impl Out {
    fn decl(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }
    fn line(&mut self, name: &str, labels: &str, v: impl std::fmt::Display) {
        let _ = writeln!(self.0, "{name}{labels} {v}");
    }
}

impl Metrics {
    /// Render the textfile. `links` are the managed links (labels + live ping
    /// statistics, `None` when the link has no probe task yet).
    pub fn render(&self, links: &[(Labels, Option<LinkStats>)]) -> String {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut o = Out(String::new());

        o.decl("mielofon_agent_up", "gauge", "Agent is up and reporting.");
        o.line("mielofon_agent_up", "", 1);
        o.decl(
            "mielofon_agent_links_configured",
            "gauge",
            "Number of links the agent discovered (autodiscover).",
        );
        o.line("mielofon_agent_links_configured", "", links.len());

        o.decl(
            "mielofon_agent_commands_total",
            "counter",
            "Commands drained from the controller, by outcome.",
        );
        for (k, v) in [
            ("received", &c.commands_received),
            ("succeeded", &c.commands_succeeded),
            ("errored", &c.commands_errored),
        ] {
            o.line(
                "mielofon_agent_commands_total",
                &format!("{{result=\"{k}\"}}"),
                get(v),
            );
        }
        o.decl(
            "mielofon_agent_probe_total",
            "counter",
            "Probe runs (always-tier harvests / gated throughput).",
        );
        for (k, v) in [
            ("ping", &c.probe_ping),
            ("iperf", &c.probe_iperf),
            ("busy", &c.probe_busy),
        ] {
            o.line(
                "mielofon_agent_probe_total",
                &format!("{{kind=\"{k}\"}}"),
                get(v),
            );
        }
        o.decl(
            "mielofon_agent_probe_errors_total",
            "counter",
            "Probe runs that produced no usable result, by kind.",
        );
        for (k, v) in [
            ("ping", &c.probe_errors_ping),
            ("iperf", &c.probe_errors_iperf),
        ] {
            o.line(
                "mielofon_agent_probe_errors_total",
                &format!("{{kind=\"{k}\"}}"),
                get(v),
            );
        }
        o.decl(
            "mielofon_agent_apply_cost_total",
            "counter",
            "OSPF cost applies by outcome.",
        );
        o.line(
            "mielofon_agent_apply_cost_total",
            "{result=\"ok\"}",
            get(&c.apply_cost),
        );
        o.line(
            "mielofon_agent_apply_cost_total",
            "{result=\"error\"}",
            get(&c.apply_cost_errors),
        );

        let thr = self.throughput.lock().expect("metrics lock");
        type Gauge = (&'static str, &'static str, &'static str);
        let per_link: [Gauge; 11] = [
            (
                "mielofon_agent_link_configured",
                "gauge",
                "Link discovered/managed by the agent (1 if present).",
            ),
            (
                "mielofon_agent_link_rtt_ms",
                "gauge",
                "Rolling mean RTT (ms).",
            ),
            (
                "mielofon_agent_link_jitter_ms",
                "gauge",
                "RTT jitter: stddev of the ping window (ms).",
            ),
            (
                "mielofon_agent_link_loss_pct",
                "gauge",
                "Packet loss over the ping window (%).",
            ),
            (
                "mielofon_agent_pings_sent_total",
                "counter",
                "Echo requests sent.",
            ),
            (
                "mielofon_agent_pings_received_total",
                "counter",
                "Echo replies received.",
            ),
            (
                "mielofon_agent_pings_lost_total",
                "counter",
                "Echo requests unanswered or unsendable.",
            ),
            (
                "mielofon_agent_link_tcp_mbps",
                "gauge",
                "Latest TCP throughput (Mbps).",
            ),
            (
                "mielofon_agent_link_util_mbps",
                "gauge",
                "Link utilization at the last throughput gate (Mbps).",
            ),
            (
                "mielofon_agent_link_busy",
                "gauge",
                "Link busy at last throughput probe (1 if busy).",
            ),
            (
                "mielofon_agent_last_probe_unixtime",
                "gauge",
                "Latest throughput probe timestamp (unix seconds).",
            ),
        ];
        for (name, kind, help) in per_link {
            o.decl(name, kind, help);
            for (labels, stats) in links {
                let l = lbl(labels);
                let s = stats.as_ref();
                let t = thr.get(&labels.2);
                let v: Option<String> = match name {
                    "mielofon_agent_link_configured" => Some("1".into()),
                    "mielofon_agent_link_rtt_ms" => {
                        s.and_then(|s| s.snapshot.rtt_ms).map(|v| v.to_string())
                    }
                    "mielofon_agent_link_jitter_ms" => {
                        s.and_then(|s| s.snapshot.jitter_ms).map(|v| v.to_string())
                    }
                    "mielofon_agent_link_loss_pct" => {
                        s.and_then(|s| s.snapshot.loss_pct).map(|v| v.to_string())
                    }
                    "mielofon_agent_pings_sent_total" => s.map(|s| s.sent.to_string()),
                    "mielofon_agent_pings_received_total" => s.map(|s| s.received.to_string()),
                    "mielofon_agent_pings_lost_total" => s.map(|s| s.lost.to_string()),
                    "mielofon_agent_link_tcp_mbps" => {
                        t.and_then(|t| t.tcp_mbps).map(|v| v.to_string())
                    }
                    "mielofon_agent_link_util_mbps" => t.map(|t| t.util_mbps.to_string()),
                    "mielofon_agent_link_busy" => t.map(|t| u8::from(t.busy).to_string()),
                    "mielofon_agent_last_probe_unixtime" => t.map(|t| t.unixtime.to_string()),
                    _ => None,
                };
                if let Some(v) = v {
                    o.line(name, &l, v);
                }
            }
        }
        o.0
    }
}

/// Atomic textfile write (temp + rename), so the collector never reads a
/// half-written file.
pub fn write_textfile(path: &str, body: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::Snapshot;

    #[test]
    fn renders_measured_values_only() {
        let m = Metrics::default();
        Counters::inc(&m.counters.commands_received);
        m.throughput.lock().unwrap().insert(
            "awg_hub_a".into(),
            LastThroughput {
                tcp_mbps: Some(87.5),
                util_mbps: 0.25,
                busy: false,
                unixtime: 1_700_000_000,
            },
        );
        let labels = |i: &str| ("node-0".to_string(), "hub-a".to_string(), i.to_string());
        let stats = LinkStats {
            snapshot: Snapshot {
                rtt_ms: Some(12.5),
                jitter_ms: None,
                loss_pct: Some(0.0),
            },
            sent: 10,
            received: 10,
            lost: 0,
        };
        let text = m.render(&[
            (labels("awg_hub_a"), Some(stats)),
            (labels("awg_hub_\"b"), None),
        ]);
        assert!(text.contains("mielofon_agent_commands_total{result=\"received\"} 1"));
        assert!(text.contains(
            "mielofon_agent_link_rtt_ms{from=\"node-0\",to=\"hub-a\",interface=\"awg_hub_a\"} 12.5"
        ));
        assert!(
            !text.contains("mielofon_agent_link_jitter_ms{"),
            "unmeasured stays absent"
        );
        assert!(text.contains("mielofon_agent_link_tcp_mbps{from=\"node-0\",to=\"hub-a\",interface=\"awg_hub_a\"} 87.5"));
        assert!(
            text.contains("interface=\"awg_hub_\\\"b\"} 1"),
            "escaped label, configured gauge"
        );
        assert_eq!(
            text.matches("# TYPE mielofon_agent_link_rtt_ms gauge")
                .count(),
            1
        );
    }
}
