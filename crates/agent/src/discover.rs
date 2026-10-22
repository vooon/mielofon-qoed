//! Link auto-discovery from BIRD/OSPF (see `docs/link-autodiscovery.md`).
//!
//! The managed link set is derived from two structured ubus snapshots:
//!
//! - `bird status` (rpcd-mod-bird): the mesh OSPF protocol (picked by name —
//!   a node may run an unrelated second OSPF instance), its point-to-point
//!   interfaces, and BGP peer protocols `peer_<hostname>` that name the node
//!   at the other end;
//! - `network.interface.<iface> status` (netifd): the interface's own global
//!   IPv6 is the probe source, the far side of its /127 the probe target;
//! - `ospf[].neighbors[]` of the same snapshot: the adjacency's link-local
//!   address (`ip`), the preferred probe target. Pinged with the interface
//!   scope it cannot leave the tunnel in either direction, whereas the global
//!   /127 address is reached by whatever path OSPF currently prefers.
//!
//! The peer behind an interface is resolved by router id first: the OSPF
//! neighbour's `rid` on that interface is the BGP peer's `neighbor_id`
//! (one router, one id), so no naming convention is needed. While the
//! adjacency is down the previously resolved peer is kept, and as a last
//! resort the interface name's token (what the glob's `*` matched) is
//! looked up as a peer's first host label (`awg_hub1` → `hub1`).

use crate::bus::Bus;
use crate::config::Config;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::Ipv6Addr;
use tracing::{debug, warn};

/// A discovered outgoing link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub interface: String,
    /// Peer node: the full hostname from its BGP protocol name.
    pub to: String,
    /// Our address on the tunnel (probe source).
    pub source: Ipv6Addr,
    /// The peer's address on the tunnel (probe target when `peer_ll` is unset).
    pub target: Ipv6Addr,
    /// The OSPF neighbour's link-local address on this interface while the
    /// adjacency is up: the probe target that stays on the tunnel.
    pub peer_ll: Option<Ipv6Addr>,
    /// The OSPF cost BIRD currently has on the interface (`None` when the
    /// snapshot does not report it), so an applied cost that a BIRD config
    /// redeploy reset can be noticed and re-applied.
    pub cost: Option<u32>,
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

/// Shell-style match of `name` against `pattern` (`*` any run, `?` one
/// char; nothing else is special).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some(b'*') => (0..=n.len()).any(|k| go(&p[1..], &n[k..])),
            Some(b'?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// What the single `*` of `pattern` matched in `name`: the peer token of a
/// conventionally named interface (`awg_*` on `awg_hub1` → `hub1`). None
/// when the pattern has no or several wildcards, or does not match.
pub fn glob_token<'a>(pattern: &str, name: &'a str) -> Option<&'a str> {
    let (pre, suf) = pattern.split_once('*')?;
    if suf.contains('*') || pre.contains('?') || suf.contains('?') {
        return None;
    }
    let mid = name.strip_prefix(pre)?.strip_suffix(suf)?;
    (!mid.is_empty()).then_some(mid)
}

/// The interface's peer token under the first glob that matches it.
pub fn iface_token<'a>(iface: &'a str, globs: &[String]) -> Option<&'a str> {
    globs
        .iter()
        .find(|g| glob_match(g, iface))
        .and_then(|g| glob_token(g, iface))
}

/// Candidate `(interface, peer hostname)` pairs from `bird status`: the
/// mesh protocol's point-to-point interfaces selected by the configured
/// globs, each resolved to its peer by router id, then by what it resolved
/// to last time (`known`), then by the interface name's token.
pub fn select_links(
    status: &Value,
    cfg: &Config,
    known: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let Some(ospf) = status["ospf"].as_array().and_then(|a| {
        a.iter()
            .find(|o| o["protocol"] == cfg.ospf_protocol.as_str())
    }) else {
        return Vec::new();
    };

    let peers: Vec<(String, Option<String>)> = status["bgp"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let host = b["name"].as_str().and_then(peer_hostname)?;
            Some((host, b["neighbor_id"].as_str().map(str::to_string)))
        })
        .collect();
    let by_label: HashMap<&str, &str> = peers
        .iter()
        .map(|(h, _)| (peer_short_label(h), h.as_str()))
        .collect();
    let by_rid: HashMap<&str, &str> = peers
        .iter()
        .filter_map(|(h, rid)| Some((rid.as_deref()?, h.as_str())))
        .collect();
    let rid_of: HashMap<&str, &str> = ospf["neighbors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| Some((n["interface"].as_str()?, n["rid"].as_str()?)))
        .collect();

    let mut out = Vec::new();
    for i in ospf["interfaces"].as_array().into_iter().flatten() {
        let Some(iface) = i["interface"].as_str() else {
            continue;
        };
        // rpcd-mod-bird >= 0.4.0 reports `type`; older snapshots rely on the
        // peer match alone.
        if let Some(t) = i["type"].as_str() {
            if t != "ptp" {
                continue;
            }
        }
        if !cfg.ifaces.iter().any(|g| glob_match(g, iface)) {
            continue;
        }
        if cfg.excludes.iter().any(|e| e == iface) {
            continue;
        }
        let peer = rid_of
            .get(iface)
            .and_then(|rid| by_rid.get(rid))
            .map(|h| h.to_string())
            .or_else(|| known.get(iface).cloned())
            .or_else(|| {
                iface_token(iface, &cfg.ifaces)
                    .and_then(|t| by_label.get(t))
                    .map(|h| h.to_string())
            });
        match peer {
            Some(host) => out.push((iface.to_string(), host)),
            None => debug!("link discovery: {iface}: no peer resolvable, skipped"),
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

fn is_link_local(a: &Ipv6Addr) -> bool {
    a.segments()[0] & 0xffc0 == 0xfe80
}

fn is_global(a: &Ipv6Addr) -> bool {
    !is_link_local(a)
}

/// Current OSPF cost per interface of the mesh protocol, from
/// `ospf[].interfaces[].cost`.
pub fn interface_costs(status: &Value, cfg: &Config) -> HashMap<String, u32> {
    let Some(ospf) = status["ospf"].as_array().and_then(|a| {
        a.iter()
            .find(|o| o["protocol"] == cfg.ospf_protocol.as_str())
    }) else {
        return HashMap::new();
    };
    ospf["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let iface = i["interface"].as_str()?;
            let cost = u32::try_from(i["cost"].as_u64()?).ok()?;
            Some((iface.to_string(), cost))
        })
        .collect()
}

/// Link-local address of the OSPF neighbour per interface, from the mesh
/// protocol's `neighbors[]` rows (BIRD prints the OSPFv3 neighbour's
/// link-local as its IP). Older rpcd-mod-bird snapshots have no rows.
pub fn neighbor_addrs(status: &Value, cfg: &Config) -> HashMap<String, Ipv6Addr> {
    let Some(ospf) = status["ospf"].as_array().and_then(|a| {
        a.iter()
            .find(|o| o["protocol"] == cfg.ospf_protocol.as_str())
    }) else {
        return HashMap::new();
    };
    ospf["neighbors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let iface = n["interface"].as_str()?;
            let ip = n["ip"].as_str()?.parse::<Ipv6Addr>().ok()?;
            is_link_local(&ip).then(|| (iface.to_string(), ip))
        })
        .collect()
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
pub async fn discover(bus: &impl Bus, cfg: &Config, known: &HashMap<String, String>) -> Vec<Link> {
    let status = match bus.call("bird", "status", json!({})).await {
        Ok(v) => v,
        Err(e) => {
            warn!("link discovery: {e:#}");
            return Vec::new();
        }
    };

    let neighbors = neighbor_addrs(&status, cfg);
    let costs = interface_costs(&status, cfg);
    let mut links = Vec::new();
    for (interface, to) in select_links(&status, cfg, known) {
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
                peer_ll: neighbors.get(&interface).copied(),
                cost: costs.get(&interface).copied(),
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
                "endpoint": "203.0.113.1:9552",
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
                { "name": "peer_hub1_example_com", "neighbor_id": "192.0.2.1" },
                { "name": "peer_hub2_example_com", "neighbor_id": "192.0.2.2" },
                { "name": "peer_hub3_example_com", "neighbor_id": "192.0.2.3" }
            ]
        })
    }

    fn none() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn interface_costs_of_the_mesh_protocol() {
        let c = interface_costs(&status(), &cfg());
        assert_eq!(c.get("awg_hub1"), Some(&10));
        assert_eq!(c.get("dummy_awg"), Some(&10));
        assert!(!c.contains_key("eth2"), "other protocol, no cost");
    }

    #[test]
    fn neighbor_link_locals_per_interface() {
        let mut st = status();
        st["ospf"][0]["neighbors"] = json!([
            { "rid": "192.0.2.1", "priority": 1, "state": "Full", "position": "PtP",
              "interface": "awg_hub1", "ip": "fe80::1" },
            // A global address here is not an on-link target.
            { "rid": "192.0.2.2", "priority": 1, "state": "Full", "position": "PtP",
              "interface": "awg_hub2", "ip": "fd00:0:0:1::2" }
        ]);
        let n = neighbor_addrs(&st, &cfg());
        assert_eq!(n.get("awg_hub1"), Some(&ip("fe80::1")));
        assert!(!n.contains_key("awg_hub2"));
        assert!(
            neighbor_addrs(&status(), &cfg()).is_empty(),
            "no rows: empty"
        );
    }

    #[test]
    fn selects_mesh_ptp_links_matched_to_bgp_peers() {
        let links = select_links(&status(), &cfg(), &none());
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
    fn peer_resolves_by_router_id_then_memory_then_name_token() {
        // A hub: its tunnel towards a spoke is named by role, not by the
        // peer's host label, so only the router id can resolve it.
        let mut st = status();
        st["ospf"][0]["interfaces"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "interface": "awg_sp_x1", "type": "ptp", "cost": 1010 }));
        st["bgp"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "name": "peer_x1spoke_example_com", "neighbor_id": "192.0.2.9" }));
        let mut v = json!({ "main": {
            "endpoint": "203.0.113.1:9552", "cacert": "ca", "cert": "c", "key": "k",
            "iface": ["awg_*", "awg_sp_*"]
        }});
        let cfg = Config::from_uci(&v, "hub-a").unwrap();

        // Adjacency down, never seen: the name token "x1" matches no label.
        let links = select_links(&st, &cfg, &none());
        assert!(
            !links.iter().any(|(i, _)| i == "awg_sp_x1"),
            "unresolvable without an adjacency: {links:?}"
        );

        // Adjacency up: router id resolves it.
        st["ospf"][0]["neighbors"] = json!([
            { "rid": "192.0.2.9", "state": "Full", "interface": "awg_sp_x1", "ip": "fe80::9" }
        ]);
        let links = select_links(&st, &cfg, &none());
        assert!(links.contains(&("awg_sp_x1".to_string(), "x1spoke.example.com".to_string())));

        // Adjacency down again: the last resolution is kept.
        st["ospf"][0]["neighbors"] = json!([]);
        let known: HashMap<String, String> =
            [("awg_sp_x1".to_string(), "x1spoke.example.com".to_string())].into();
        let links = select_links(&st, &cfg, &known);
        assert!(links.contains(&("awg_sp_x1".to_string(), "x1spoke.example.com".to_string())));

        // Router id wins over a stale memory and over the name token.
        st["ospf"][0]["neighbors"] = json!([
            { "rid": "192.0.2.3", "state": "Full", "interface": "awg_hub1", "ip": "fe80::3" }
        ]);
        let stale: HashMap<String, String> =
            [("awg_hub1".to_string(), "old.example.com".to_string())].into();
        let links = select_links(&st, &cfg, &stale);
        assert!(links.contains(&("awg_hub1".to_string(), "hub3.example.com".to_string())));
        v["main"]["iface"] = json!("eth*");
        let cfg = Config::from_uci(&v, "hub-a").unwrap();
        assert!(
            select_links(&st, &cfg, &none()).is_empty(),
            "glob selects nothing"
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
        assert_eq!(select_links(&st, &cfg(), &none()).len(), 1);
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
        let hub = vec!["awg_int_*".to_string(), "awg_sp_*".to_string()];
        assert_eq!(
            iface_token("awg_hub1", &["awg_*".to_string()]),
            Some("hub1")
        );
        assert_eq!(iface_token("eth0", &["awg_*".to_string()]), None);
        assert_eq!(iface_token("awg_int_hub2", &hub), Some("hub2"));
        assert_eq!(iface_token("awg_sp_node0", &hub), Some("node0"));
        assert_eq!(iface_token("awg_hub1", &hub), None, "no glob matched");
        assert!(glob_match("awg_*", "awg_"));
        assert!(glob_match("*_tun", "hub1_tun"));
        assert!(glob_match("awg_??", "awg_h1"));
        assert!(!glob_match("awg_?", "awg_h1"));
        assert!(!glob_match("awg_*", "wg_hub1"));
        assert_eq!(glob_token("*_tun", "hub1_tun"), Some("hub1"));
        assert_eq!(glob_token("a*b*", "axbx"), None, "two wildcards: no token");
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
        let mut st = status();
        st["ospf"][0]["neighbors"] = json!([
            { "rid": "192.0.2.1", "priority": 1, "state": "Full", "position": "PtP",
              "interface": "awg_hub1", "ip": "fe80::1" }
        ]);
        bus.on("bird.status", Ok(st));
        bus.on(
            "network.interface.awg_hub1.status",
            Ok(json!({ "ipv6-address": [{ "address": "fd00:0:0:1::40:1", "mask": 127 }]})),
        );
        // awg_hub2 has no global address yet: skipped, not fatal.
        bus.on(
            "network.interface.awg_hub2.status",
            Ok(json!({ "ipv6-address": [{ "address": "fe80::1", "mask": 64 }]})),
        );
        let links = discover(&bus, &cfg(), &none()).await;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].to, "hub1.example.com");
        assert_eq!(links[0].target, ip("fd00:0:0:1::40:0"));
        assert_eq!(links[0].peer_ll, Some(ip("fe80::1")));

        let dead = FakeBus::default();
        assert!(discover(&dead, &cfg(), &none()).await.is_empty());
    }
}
