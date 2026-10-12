//! Link auto-discovery from BIRD/OSPF (see `docs/link-autodiscovery.md`).
//!
//! The managed link set is derived from two structured ubus snapshots:
//!
//! - `bird status` (rpcd-mod-bird): the mesh OSPF protocol (picked by name —
//!   a node may run an unrelated second OSPF instance), its point-to-point
//!   interfaces, and BGP peer protocols `peer_<hostname>` that name the node
//!   at the other end;
//! - `network.interface.<iface> status` (netifd): the interface's own global
//!   IPv6 is the probe source, the far side of its /127 the probe target.

use crate::bus::Bus;
use crate::config::Config;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::Ipv6Addr;
use tracing::warn;

/// A discovered outgoing link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub interface: String,
    /// Peer node: the full hostname from its BGP protocol name.
    pub to: String,
    /// Our address on the tunnel (probe source).
    pub source: Ipv6Addr,
    /// The peer's address on the tunnel (probe target).
    pub target: Ipv6Addr,
}

/// The peer's full hostname from its BGP protocol name: `peer_<hostname>`
/// with dots written as underscores (`peer_hub1_example_com` →
/// `hub1.example.com`). Never shortened: it must name the same node the peer
/// registers under, so both halves of a tunnel pair up on the controller.
pub fn peer_hostname(name: &str) -> Option<String> {
    let host = name.strip_prefix("peer_")?;
    (!host.is_empty()).then(|| host.replace('_', "."))
}

/// First dot-label of a hostname, the token tunnel interfaces are named by.
pub fn peer_short_label(host: &str) -> &str {
    host.split('.').next().unwrap_or(host)
}

/// Interface token by the prefix convention (`awg_hub1` → `hub1`).
pub fn iface_token<'a>(iface: &'a str, prefix: &str) -> &'a str {
    iface.strip_prefix(prefix).unwrap_or(iface)
}

/// Candidate `(interface, peer hostname)` pairs from `bird status`.
pub fn select_links(status: &Value, cfg: &Config) -> Vec<(String, String)> {
    let Some(ospf) = status["ospf"].as_array().and_then(|a| {
        a.iter()
            .find(|o| o["protocol"] == cfg.ospf_protocol.as_str())
    }) else {
        return Vec::new();
    };

    let nodes: HashMap<String, String> = status["bgp"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["name"].as_str().and_then(peer_hostname))
        .map(|h| (peer_short_label(&h).to_string(), h))
        .collect();

    let mut out = Vec::new();
    for i in ospf["interfaces"].as_array().into_iter().flatten() {
        let Some(iface) = i["interface"].as_str() else {
            continue;
        };
        // rpcd-mod-bird >= 0.4.0 reports `type`; older snapshots rely on the
        // BGP peer match alone.
        if let Some(t) = i["type"].as_str() {
            if t != "ptp" {
                continue;
            }
        }
        if cfg.excludes.iter().any(|e| e == iface) {
            continue;
        }
        if let Some(host) = nodes.get(iface_token(iface, &cfg.iface_prefix)) {
            out.push((iface.to_string(), host.clone()));
        }
    }
    out
}

fn ipv6_addresses(iface_status: &Value) -> impl Iterator<Item = (Ipv6Addr, Option<u64>)> + '_ {
    iface_status["ipv6-address"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let addr = a["address"].as_str()?.parse::<Ipv6Addr>().ok()?;
            Some((addr, a["mask"].as_u64()))
        })
}

fn is_global(a: &Ipv6Addr) -> bool {
    // Not link-local (fe80::/10).
    a.segments()[0] & 0xffc0 != 0xfe80
}

/// `(source, target)` for a tunnel: our global /127 address (else any global
/// one) and the other address of that /127.
pub fn derive_address(iface_status: &Value) -> Option<(Ipv6Addr, Ipv6Addr)> {
    let source = ipv6_addresses(iface_status)
        .find(|(a, mask)| *mask == Some(127) && is_global(a))
        .or_else(|| ipv6_addresses(iface_status).find(|(a, _)| is_global(a)))?
        .0;
    let target = Ipv6Addr::from(u128::from(source) ^ 1);
    Some((source, target))
}

/// The node's mesh loopback: the first global IPv6 on the loopback interface.
pub fn loopback_address(iface_status: &Value) -> Option<Ipv6Addr> {
    ipv6_addresses(iface_status).map(|(a, _)| a).find(is_global)
}

/// Discover the managed links. Interfaces without a usable address are
/// skipped with a warning; a failed `bird status` yields an empty set (the
/// next refresh retries).
pub async fn discover(bus: &impl Bus, cfg: &Config) -> Vec<Link> {
    let status = match bus.call("bird", "status", json!({})).await {
        Ok(v) => v,
        Err(e) => {
            warn!("link discovery: {e:#}");
            return Vec::new();
        }
    };

    let mut links = Vec::new();
    for (interface, to) in select_links(&status, cfg) {
        let object = format!("network.interface.{interface}");
        let addr = match bus.call(&object, "status", json!({})).await {
            Ok(st) => derive_address(&st),
            Err(e) => {
                warn!("link discovery: {interface}: {e:#}");
                continue;
            }
        };
        match addr {
            Some((source, target)) => links.push(Link {
                interface,
                to,
                source,
                target,
            }),
            None => warn!("link discovery: {interface}: no global IPv6, skipped"),
        }
    }
    links.sort_by(|a, b| a.interface.cmp(&b.interface));
    links
}

/// The mesh loopback of this node, if its interface is up.
pub async fn loopback(bus: &impl Bus, cfg: &Config) -> Option<Ipv6Addr> {
    let object = format!("network.interface.{}", cfg.loopback_iface);
    let st = bus.call(&object, "status", json!({})).await.ok()?;
    loopback_address(&st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::fake::FakeBus;

    fn ip(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    fn cfg() -> Config {
        let v = json!({
            "main": {
                "controller_url": "https://203.0.113.1:9552",
                "cacert": "ca", "cert": "c", "key": "k"
            },
            "x": { ".type": "exclude", "interface": "awg_legacy" }
        });
        Config::from_uci(&v, "node-0").unwrap()
    }

    /// A node with two OSPF instances: the mesh (ptp awg_* plus a broadcast
    /// stub) and an unrelated IPv4 instance on eth2/eth3.
    fn status() -> Value {
        json!({
            "ospf": [
                { "protocol": "mesh_v3", "interfaces": [
                    { "interface": "dummy_awg", "type": "broadcast", "cost": 10 },
                    { "interface": "awg_hub1", "type": "ptp", "cost": 10 },
                    { "interface": "awg_hub2", "type": "ptp", "cost": 10 },
                    { "interface": "awg_legacy", "type": "ptp", "cost": 10 }
                ]},
                { "protocol": "office_v2", "interfaces": [
                    { "interface": "eth2", "type": "ptp" },
                    { "interface": "eth3", "type": "ptp" }
                ]}
            ],
            "bgp": [
                { "name": "peer_hub1_example_com" },
                { "name": "peer_hub2_example_com" },
                { "name": "peer_hub3_example_com" }
            ]
        })
    }

    #[test]
    fn selects_mesh_ptp_links_matched_to_bgp_peers() {
        let links = select_links(&status(), &cfg());
        assert_eq!(
            links,
            vec![
                ("awg_hub1".to_string(), "hub1.example.com".to_string()),
                ("awg_hub2".to_string(), "hub2.example.com".to_string()),
            ],
            "broadcast stub, excluded iface, other OSPF and peer without iface dropped"
        );
    }

    #[test]
    fn untyped_interfaces_fall_back_to_peer_match() {
        let st = json!({
            "ospf": [{ "protocol": "mesh_v3", "interfaces": [
                { "interface": "awg_hub1" }, { "interface": "dummy_awg" }
            ]}],
            "bgp": [{ "name": "peer_hub1_example_com" }]
        });
        assert_eq!(select_links(&st, &cfg()).len(), 1);
    }

    #[test]
    fn naming_helpers() {
        assert_eq!(
            peer_hostname("peer_hub1_example_com").as_deref(),
            Some("hub1.example.com")
        );
        assert_eq!(peer_hostname("peer_nr").as_deref(), Some("nr"));
        assert_eq!(peer_hostname("other_proto"), None);
        assert_eq!(peer_hostname("peer_"), None);
        assert_eq!(peer_short_label("hub1.example.com"), "hub1");
        assert_eq!(peer_short_label("nr"), "nr");
        assert_eq!(iface_token("awg_hub1", "awg_"), "hub1");
        assert_eq!(iface_token("eth0", "awg_"), "eth0");
    }

    #[test]
    fn derives_far_side_of_the_127() {
        let st = json!({ "ipv6-address": [
            { "address": "fe80::4243:40:1", "mask": 64 },
            { "address": "fd00:0:0:1::40:1", "mask": 127 }
        ]});
        let (src, dst) = derive_address(&st).unwrap();
        assert_eq!(src, ip("fd00:0:0:1::40:1"));
        assert_eq!(dst, ip("fd00:0:0:1::40:0"));

        let lower = json!({ "ipv6-address": [{ "address": "fd00:0:0:1::40:0", "mask": 127 }]});
        assert_eq!(derive_address(&lower).unwrap().1, ip("fd00:0:0:1::40:1"));

        let ll_only = json!({ "ipv6-address": [{ "address": "fe80::1", "mask": 64 }]});
        assert_eq!(derive_address(&ll_only), None);
        assert_eq!(derive_address(&Value::Null), None);
    }

    #[test]
    fn loopback_is_first_global() {
        let st = json!({ "ipv6-address": [
            { "address": "fe80::1", "mask": 64 },
            { "address": "fd00:0:0:1::2", "mask": 128 }
        ]});
        assert_eq!(loopback_address(&st), Some(ip("fd00:0:0:1::2")));
        assert_eq!(loopback_address(&json!({})), None);
    }

    #[tokio::test]
    async fn discover_end_to_end() {
        let bus = FakeBus::default();
        bus.on("bird.status", Ok(status()));
        bus.on(
            "network.interface.awg_hub1.status",
            Ok(json!({ "ipv6-address": [{ "address": "fd00:0:0:1::40:1", "mask": 127 }]})),
        );
        // awg_hub2 has no global address yet: skipped, not fatal.
        bus.on(
            "network.interface.awg_hub2.status",
            Ok(json!({ "ipv6-address": [{ "address": "fe80::1", "mask": 64 }]})),
        );
        let links = discover(&bus, &cfg()).await;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].to, "hub1.example.com");
        assert_eq!(links[0].target, ip("fd00:0:0:1::40:0"));

        let dead = FakeBus::default();
        assert!(discover(&dead, &cfg()).await.is_empty());
    }
}
