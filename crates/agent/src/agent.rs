//! Node agent: keeps the discovered link set and the probes in sync, and
//! executes the controller's commands. A thin executor — scheduling, the
//! probe fence and cost policy all live in the controller.

use crate::bird;
use crate::bus::Bus;
use crate::config::Config;
use crate::discover::{self, Link};
use crate::icmp::{Gate, Prober};
use crate::metrics::{Counters, LastThroughput, Metrics};
use crate::throughput::Throughput;
use mielofon_proto::{AgentReply, LinkKey, ProbeState, RegisterReq, Tier, WorkCmd};
use std::net::Ipv6Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

#[derive(Default)]
struct NodeState {
    links: Vec<Link>,
    loopback: Option<Ipv6Addr>,
}

pub struct Agent<B> {
    pub cfg: Config,
    pub bus: B,
    pub prober: Prober,
    pub throughput: Throughput,
    pub metrics: Metrics,
    /// Delay between `set_ospf_cost` retries while BIRD is reconfiguring.
    pub cost_retry: Duration,
    state: Mutex<NodeState>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl<B: Bus> Agent<B> {
    pub fn new(cfg: Config, bus: B) -> Self {
        let gate = Arc::new(Gate::default());
        // Let the queue the test built up drain before sampling resumes.
        let grace = (cfg.ping_interval * 2).max(Duration::from_secs(2));
        Agent {
            prober: Prober::new(cfg.ping_interval, cfg.ping_window, gate.clone()),
            throughput: Throughput::new(
                cfg.iperf_bin.clone(),
                cfg.iperf_port,
                cfg.tcp_duration,
                cfg.quiet_max_mbps,
                grace,
                gate,
            ),
            metrics: Metrics::default(),
            cost_retry: Duration::from_secs(2),
            state: Mutex::new(NodeState::default()),
            cfg,
            bus,
        }
    }

    /// Re-discover links and the mesh loopback, and reconcile the probes.
    pub async fn refresh(&self) {
        let links = discover::discover(&self.bus, &self.cfg).await;
        let loopback = discover::loopback(&self.bus, &self.cfg).await;
        self.prober.reconcile(&links);
        let mut st = self.state.lock().expect("agent state");
        if st.links != links {
            info!(
                "links: {}",
                links
                    .iter()
                    .map(|l| format!("{}→{}", l.interface, l.to))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        st.links = links;
        st.loopback = loopback;
    }

    fn key(&self, l: &Link) -> LinkKey {
        LinkKey::new(&self.cfg.agent, &l.to, &l.interface)
    }

    /// The managed links as the controller knows them.
    pub fn link_keys(&self) -> Vec<LinkKey> {
        let st = self.state.lock().expect("agent state");
        st.links.iter().map(|l| self.key(l)).collect()
    }

    pub fn register_req(&self) -> RegisterReq {
        let loopback = self.state.lock().expect("agent state").loopback;
        RegisterReq {
            agent: self.cfg.agent.clone(),
            links: self.link_keys(),
            loopback: loopback.map(|a| a.to_string()),
        }
    }

    fn find(&self, iface: &str) -> Option<Link> {
        let st = self.state.lock().expect("agent state");
        st.links.iter().find(|l| l.interface == iface).cloned()
    }

    /// Execute one command; the reply to post, if any.
    pub async fn execute(&self, cmd: WorkCmd) -> Option<AgentReply> {
        let c = &self.metrics.counters;
        Counters::inc(&c.commands_received);
        let agent = self.cfg.agent.clone();

        match cmd {
            WorkCmd::Probe {
                id,
                tier,
                token,
                link,
                traceparent,
            } => {
                let Some(l) = self.find(&link.interface) else {
                    Counters::inc(&c.commands_errored);
                    warn!("probe {id}: unknown interface {}", link.interface);
                    // A throughput command holds the controller's fence: answer
                    // busy so the lease is released now, not at its TTL.
                    return (tier == Tier::Throughput).then_some(AgentReply::Probe {
                        agent,
                        id,
                        link,
                        ts: None,
                        rtt_ms: None,
                        loss_pct: None,
                        jitter_ms: None,
                        tcp_mbps: None,
                        util_mbps: 0.0,
                        state: ProbeState::Busy,
                        token,
                        traceparent,
                    });
                };

                let (util, tcp, state, token) = if tier == Tier::Throughput {
                    Counters::inc(&c.probe_iperf);
                    let out = self.throughput.run(&l).await;
                    if out.busy {
                        Counters::inc(&c.probe_busy);
                    }
                    if out.tcp_mbps.is_none() && out.util_mbps <= self.cfg.quiet_max_mbps {
                        Counters::inc(&c.probe_errors_iperf);
                    }
                    self.metrics
                        .throughput
                        .lock()
                        .expect("metrics lock")
                        .insert(
                            l.interface.clone(),
                            LastThroughput {
                                tcp_mbps: out.tcp_mbps,
                                util_mbps: out.util_mbps,
                                busy: out.busy,
                                unixtime: unix_now(),
                            },
                        );
                    let state = if out.busy {
                        ProbeState::Busy
                    } else {
                        ProbeState::Quiet
                    };
                    (out.util_mbps, out.tcp_mbps, state, token)
                } else {
                    Counters::inc(&c.probe_ping);
                    (0.0, None, ProbeState::Quiet, None)
                };

                // Always-tier dims come from the live ping window — for a
                // throughput reply too, so they stay populated across it.
                let snap = self
                    .prober
                    .stats(&l.interface)
                    .map(|s| s.snapshot)
                    .unwrap_or_default();
                if tier == Tier::Always && snap.rtt_ms.is_none() {
                    Counters::inc(&c.probe_errors_ping);
                }
                Counters::inc(&c.commands_succeeded);
                Some(AgentReply::Probe {
                    agent,
                    id,
                    link: self.key(&l),
                    ts: None,
                    rtt_ms: snap.rtt_ms,
                    loss_pct: snap.loss_pct,
                    jitter_ms: snap.jitter_ms,
                    tcp_mbps: tcp,
                    util_mbps: util,
                    state,
                    token,
                    traceparent,
                })
            }

            WorkCmd::ApplyCost {
                id,
                link,
                cost,
                traceparent,
            } => {
                let note = if self.find(&link.interface).is_none() {
                    Some(format!("unknown interface {}", link.interface))
                } else {
                    bird::set_ospf_cost(&self.bus, &link.interface, cost, self.cost_retry)
                        .await
                        .err()
                };
                match &note {
                    None => {
                        Counters::inc(&c.apply_cost);
                        Counters::inc(&c.commands_succeeded);
                    }
                    Some(e) => {
                        warn!("apply {id}: {e}");
                        Counters::inc(&c.apply_cost_errors);
                        Counters::inc(&c.commands_errored);
                    }
                }
                Some(AgentReply::Applied {
                    agent,
                    id,
                    link,
                    cost,
                    note,
                    traceparent,
                })
            }

            WorkCmd::Trace {
                id,
                target,
                traceparent,
            } => {
                let (code, routes) = bird::route(&self.bus, &target).await;
                Counters::inc(&c.commands_succeeded);
                Some(AgentReply::Trace {
                    agent,
                    id,
                    traceparent,
                    prefix: target,
                    code,
                    routes,
                })
            }
        }
    }

    /// Render the metrics textfile from the live state.
    pub fn render_metrics(&self) -> String {
        let links: Vec<_> = {
            let st = self.state.lock().expect("agent state");
            st.links
                .iter()
                .map(|l| {
                    (
                        (self.cfg.agent.clone(), l.to.clone(), l.interface.clone()),
                        self.prober.stats(&l.interface),
                    )
                })
                .collect()
        };
        self.metrics.render(&links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::fake::FakeBus;
    use serde_json::json;

    fn agent() -> Agent<FakeBus> {
        let cfg = Config::from_uci(
            &json!({ "main": {
                "agent_name": "node-0",
                "controller_url": "https://203.0.113.1:9552",
                "cacert": "ca", "cert": "c", "key": "k",
                // Pings never reach the test's documentation-range peers.
                "ping_interval": "60"
            }}),
            "host",
        )
        .unwrap();
        let bus = FakeBus::default();
        bus.on(
            "bird.status",
            Ok(json!({
                "ospf": [{ "protocol": "mesh_v3", "interfaces": [
                    { "interface": "awg_hub", "type": "ptp" }
                ]}],
                "bgp": [{ "name": "peer_hub_example_com" }]
            })),
        );
        bus.on(
            "network.interface.awg_hub.status",
            Ok(json!({ "ipv6-address": [{ "address": "2001:db8::1", "mask": 127 }]})),
        );
        bus.on(
            "network.interface.dummy_awg.status",
            Ok(json!({ "ipv6-address": [{ "address": "2001:db8:1::1", "mask": 128 }]})),
        );
        let mut a = Agent::new(cfg, bus);
        a.cost_retry = Duration::ZERO;
        a
    }

    #[tokio::test]
    async fn registers_discovered_links_and_loopback() {
        let a = agent();
        a.refresh().await;
        let req = a.register_req();
        assert_eq!(req.agent, "node-0");
        assert_eq!(
            req.links,
            vec![LinkKey::new("node-0", "hub.example.com", "awg_hub")]
        );
        assert_eq!(req.loopback.as_deref(), Some("2001:db8:1::1"));
    }

    #[tokio::test]
    async fn always_probe_reports_unmeasured_as_null() {
        let a = agent();
        a.refresh().await;
        let r = a
            .execute(WorkCmd::Probe {
                id: "always/1".into(),
                tier: Tier::Always,
                token: None,
                link: LinkKey::new("node-0", "hub.example.com", "awg_hub"),
                traceparent: Some("00-tp".into()),
            })
            .await
            .unwrap();
        let AgentReply::Probe {
            id,
            rtt_ms,
            state,
            traceparent,
            ..
        } = r
        else {
            panic!("expected a probe reply");
        };
        assert_eq!(id, "always/1");
        assert_eq!(rtt_ms, None, "no ping yet: null, never 0");
        assert_eq!(state, ProbeState::Quiet);
        assert_eq!(traceparent.as_deref(), Some("00-tp"));
    }

    #[tokio::test]
    async fn unknown_link_throughput_releases_the_fence() {
        let a = agent();
        a.refresh().await;
        let r = a
            .execute(WorkCmd::Probe {
                id: "thr/1".into(),
                tier: Tier::Throughput,
                token: Some("tok".into()),
                link: LinkKey::new("node-0", "hub-x", "awg_gone"),
                traceparent: None,
            })
            .await;
        let Some(AgentReply::Probe { state, token, .. }) = r else {
            panic!("expected a busy reply");
        };
        assert_eq!(state, ProbeState::Busy);
        assert_eq!(token.as_deref(), Some("tok"));

        let always = a
            .execute(WorkCmd::Probe {
                id: "always/2".into(),
                tier: Tier::Always,
                token: None,
                link: LinkKey::new("node-0", "hub-x", "awg_gone"),
                traceparent: None,
            })
            .await;
        assert!(always.is_none(), "an unknown always probe is dropped");
    }

    #[tokio::test]
    async fn apply_cost_acks_with_note_on_failure() {
        let a = agent();
        a.refresh().await;
        a.bus.on(
            "bird.set_ospf_cost",
            Ok(json!({ "code": 0, "stdout": "ok" })),
        );
        let ok = a
            .execute(WorkCmd::ApplyCost {
                id: "apply/1".into(),
                link: LinkKey::new("node-0", "hub.example.com", "awg_hub"),
                cost: 20,
                traceparent: None,
            })
            .await;
        assert!(matches!(
            ok,
            Some(AgentReply::Applied {
                note: None,
                cost: 20,
                ..
            })
        ));

        let unknown = a
            .execute(WorkCmd::ApplyCost {
                id: "apply/2".into(),
                link: LinkKey::new("node-0", "hub-x", "awg_gone"),
                cost: 20,
                traceparent: None,
            })
            .await;
        assert!(matches!(
            unknown,
            Some(AgentReply::Applied { note: Some(_), .. })
        ));
    }

    #[tokio::test]
    async fn trace_passes_route_through() {
        let a = agent();
        a.bus.on(
            "bird.route",
            Ok(
                json!({ "code": 0, "routes": [{ "prefix": "2001:db8:1::3/128",
                "next_hops": [{ "kind": "dev", "iface": "dummy_awg" }] }]}),
            ),
        );
        let r = a
            .execute(WorkCmd::Trace {
                id: "trace1".into(),
                target: "2001:db8:1::3".into(),
                traceparent: None,
            })
            .await;
        let Some(AgentReply::Trace {
            code,
            routes,
            prefix,
            ..
        }) = r
        else {
            panic!("expected a trace reply");
        };
        assert_eq!(
            (code, prefix.as_str(), routes.len()),
            (0, "2001:db8:1::3", 1)
        );
    }
}
