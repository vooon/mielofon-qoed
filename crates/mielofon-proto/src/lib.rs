//! Wire types shared by the mielofon controller and agent.
//!
//! Everything that crosses the agent ↔ controller boundary lives here, so
//! both sides compile against the same contract. Messages travel as frames
//! on `mielofon-transport` streams: the agent opens a stream for an
//! [`AgentRequest`] and reads its response; the controller opens a stream
//! for a [`WorkCmd`] and reads the [`AgentReply`]. See `docs/protocol.md`.

use serde::{Deserialize, Serialize};

/// Identifies a directed link: `from`'s tunnel `interface` towards `to`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkKey {
    pub from: String,
    pub to: String,
    pub interface: String,
}

impl LinkKey {
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        interface: impl Into<String>,
    ) -> Self {
        LinkKey {
            from: from.into(),
            to: to.into(),
            interface: interface.into(),
        }
    }

    /// Compact stable identifier used as map keys and command ids.
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.from, self.to, self.interface)
    }
}

/// Probe state reported by the agent alongside measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProbeState {
    #[default]
    Quiet,
    Busy,
    Conflict,
}

/// Probe tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Always,
    Throughput,
}

/// A command the controller dispatches to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkCmd {
    Probe {
        id: String,
        tier: Tier,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        link: LinkKey,
        /// W3C `traceparent` of the dispatch span, echoed back by the agent so
        /// the reply/report spans join the same trace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
    ApplyCost {
        id: String,
        link: LinkKey,
        cost: u32,
        /// W3C `traceparent` of the dispatch span (see above).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
    /// Ask an agent to resolve `target` (an IP/prefix) against BIRD and report
    /// the `next_hops` it would transport to (the controller's trace walker).
    Trace {
        id: String,
        target: String,
        /// W3C `traceparent` of the dispatch span (see above).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
}

/// A request the agent opens a stream for. `register` is the session hello:
/// it names the agent and its links, and is repeated on the same session
/// whenever the link set changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentRequest {
    Register(RegisterReq),
}

/// The agent's identity and managed links.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterReq {
    pub agent: String,
    #[serde(default)]
    pub links: Vec<LinkKey>,
    /// Mesh loopback address (IPv6) the agent reported, used by the trace
    /// walker to resolve a destination node name into a routable target.
    #[serde(default)]
    pub loopback: Option<String>,
}

/// Answer to `register`. The policy snapshot is not carried here: the
/// controller opens `apply_cost` streams for every known cost right after.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResp {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The answer to a dispatched command, written on the command's own stream.
/// Every reply still echoes the command's job identifier (`id`) for logs
/// and traces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentReply {
    Probe {
        agent: String,
        id: String,
        link: LinkKey,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<u64>,
        /// Unmeasured dimensions are `null`, never a fabricated 0.
        #[serde(default)]
        rtt_ms: Option<f64>,
        #[serde(default)]
        loss_pct: Option<f64>,
        #[serde(default)]
        jitter_ms: Option<f64>,
        #[serde(default)]
        tcp_mbps: Option<f64>,
        /// Gated UDP leg (a fixed-rate stream): received rate, jitter and
        /// loss as the peer's iperf3 server saw them — the raw path, free of
        /// TCP's congestion control.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        udp_mbps: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        udp_jitter_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        udp_loss_pct: Option<f64>,
        util_mbps: f64,
        state: ProbeState,
        /// Fence token echoed from a throughput command.
        #[serde(default)]
        token: Option<String>,
        /// Why the probe produced no measurement (e.g. iperf3 could not reach
        /// the peer). The dimensions stay `null`; this is for the operator.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// Echo of the dispatch span's W3C `traceparent`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
    Applied {
        agent: String,
        id: String,
        link: LinkKey,
        cost: u32,
        /// Why the cost could not be applied, if it could not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// Echo of the dispatch span's W3C `traceparent`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },
    /// The hop's BIRD route resolution for a `trace` command's `target`,
    /// passed through from `rpcd-mod-bird route`. The controller owns the
    /// ECMP parsing.
    Trace {
        agent: String,
        id: String,
        /// Echo of the dispatch span's W3C `traceparent`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
        prefix: String,
        /// 0 resolved, 1 unresolved (or no `route` method), 2 bad args.
        #[serde(default)]
        code: i64,
        #[serde(default)]
        routes: Vec<RouteRep>,
    },
}

/// One BIRD route from `rpcd-mod-bird route` (passed through untouched).
/// Field names pin the feed's contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRep {
    pub prefix: String,
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_pref: Option<u32>,
    /// BGP AS path as rpcd-mod-bird emits it (ASN list); `null` for OSPF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_path: Option<Vec<u32>>,
    #[serde(default)]
    pub next_hops: Vec<NextHopRep>,
}

/// One ECMP next hop. `via addr on iface` = continue into the mesh; `dev iface`
/// = the prefix is attached locally (on the destination this is `dummy_awg`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NextHopRep {
    Via {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iface: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proto: Option<String>,
    },
    Dev {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iface: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proto: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_cmd_round_trips_with_type_tag() {
        let cmd = WorkCmd::Probe {
            id: "throughput/node-0/1".into(),
            tier: Tier::Throughput,
            token: Some("t1".into()),
            link: LinkKey::new("node-0", "hub-a", "awg_hub_a"),
            traceparent: None,
        };
        let js = serde_json::to_value(&cmd).unwrap();
        assert_eq!(js["type"], "probe");
        assert_eq!(js["tier"], "throughput");
        assert!(js.get("traceparent").is_none());
        let back: WorkCmd = serde_json::from_value(js).unwrap();
        assert!(matches!(
            back,
            WorkCmd::Probe {
                tier: Tier::Throughput,
                ..
            }
        ));
    }

    #[test]
    fn probe_reply_keeps_unmeasured_dims_null() {
        let r = AgentReply::Probe {
            agent: "node-0".into(),
            id: "always/node-0/1".into(),
            link: LinkKey::new("node-0", "hub-a", "awg_hub_a"),
            ts: None,
            rtt_ms: None,
            loss_pct: Some(0.0),
            jitter_ms: None,
            tcp_mbps: None,
            udp_mbps: None,
            udp_jitter_ms: None,
            udp_loss_pct: None,
            util_mbps: 0.0,
            state: ProbeState::Quiet,
            token: None,
            note: None,
            traceparent: None,
        };
        let js = serde_json::to_value(&r).unwrap();
        assert_eq!(js["kind"], "probe");
        assert!(js.get("note").is_none());
        assert!(js["rtt_ms"].is_null());
        assert_eq!(js["loss_pct"], 0.0);
    }

    #[test]
    fn trace_reply_parses_rpcd_route_shape() {
        let js = serde_json::json!({
            "kind": "trace", "agent": "hub-a", "id": "trace1",
            "prefix": "fd00:0:0:1::3", "code": 0,
            "routes": [{
                "prefix": "fd00:0:0:1::3/128", "primary": "via", "proto": "ospf3",
                "preference": 150, "cost": 10, "local_pref": null, "as_path": null,
                "next_hops": [
                    { "kind": "via", "addr": "fd00:0:0:1::2", "iface": "awg_hub_b", "cost": 10 },
                    { "kind": "dev", "iface": "dummy_awg", "cost": 0 }
                ]
            }]
        });
        let r: AgentReply = serde_json::from_value(js).unwrap();
        let AgentReply::Trace { routes, .. } = r else {
            panic!("not a trace reply");
        };
        assert_eq!(routes[0].next_hops.len(), 2);
        assert!(matches!(routes[0].next_hops[1], NextHopRep::Dev { .. }));
    }

    #[test]
    fn bgp_route_as_path_is_an_asn_list() {
        // rpcd-mod-bird `parseRoute`: as_path is an array of ASNs for BGP,
        // and a `dev` hop carries `addr: null`.
        let js = serde_json::json!({
            "prefix": "198.51.100.0/24", "primary": "via", "proto": "peer_hub_a",
            "preference": 100, "cost": 10, "local_pref": 100, "as_path": [64512, 64513],
            "next_hops": [{ "kind": "dev", "addr": null, "iface": "dummy_awg", "cost": 10 }]
        });
        let r: RouteRep = serde_json::from_value(js).unwrap();
        assert_eq!(r.as_path, Some(vec![64512, 64513]));
    }
}
