# Link auto-discovery from BIRD/OSPF (design, rev 2)

Status: proposed — not yet implemented.

## Problem

The agent needs a per-link `config link` section for every outgoing mesh link
(interface, peer, probe target/source, cost command). BIRD already knows the
mesh links locally, and `rpcd-mod-bird` already parses BIRD's CLI output into
structured JSON. This design **removes the per-link config entirely**: the
agent discovers its links from `bird status` (rpcd), scoped to the mesh OSPF
protocol, keeps only an **exclude list**, and sets costs through a new
dedicated rpcd method instead of a raw BIRD `configure` string.

The wire contract to the controller is untouched: `register` posts
`{from, to, interface}`, and the controller references a link solely by that
triple.

## Why rpcd, not the agent, parses BIRD

`rpcd-mod-bird`'s `status` method already runs `show ospf interface`,
`show ospf neighbor`, `show protocols all`, and parses them into JSON
(`ospf[].interfaces[]`, `ospf[].neighbors[]`, `bgp[]`, `protocols[]`). The
agent must not reimplement any of that parsing — it consumes the structured
JSON and does only a thin selection/matching.

### Required rpcd-mod-bird extensions

Small changes to the feed plugin (implemented in the private feed repo; the
agent here consumes the API):

1. **Expose per-interface `type` in `status`.** Today
   `parseOspfInterfaces()` only keeps `{interface, cost}` and drops the
   per-interface `Type: ptp|broadcast` line. Add it so the agent can select
   `type == ptp` without falling back to raw queries. (No per-interface
   `area` parsing is needed — see scoping below.)

2. **Add a dedicated cost method** (name TBD, e.g. `set_cost` / `set_ospf_cost`):

   ```
   bird set_cost { "protocol": "mesh_v3", "interface": "awg_hub_a", "cost": 25 }
   ```
   → runs the feed's BIRD reconfigure path for the given OSPF interface cost
   and returns `{ code, stdout }`. The agent no longer builds a BIRD CLI
   string (removes the per-link `cost_command` and the generic `query`
   passthrough from the agent).

## Discovery pipeline (startup, once)

```
status = ubus call bird status                      # structured, rpcd-parsed
mesh   = pick(status.ospf, proto == cfg.ospf_protocol)   # e.g. mesh_v3
if multiple OSPF protocols exist, this filter is mandatory (see below)

candidate ifaces = filter(mesh.interfaces, type == 'ptp')

for each iface in candidate ifaces:
    if iface.interface in exclude list: skip
    peer = match_bgp_peer(iface.interface)          # naming convention, below
    if peer == null: skip + WARN
    addrs = ubus call network.interface.<iface> status   # no `ip` needed
    source = first global IPv6 addr on the iface    # address+mask from JSON
    target = far end of the /127 (flip last address bit of source)
    links += { from: agent_name, to: peer.node_name,
               interface, target, source }
```

### Multi-OSPF scoping (mandatory filter)

Several nodes run a second, unrelated OSPF instance (e.g. an office/underlay
protocol on `eth2`/`eth3`). Auto-discovery is scoped by the OSPF **protocol
name** — `cfg.ospf_protocol` (e.g. `mesh_v3`) picks the matching
`status.ospf[]` entry, and only its interfaces are considered:

```
mesh = pick(status.ospf, proto == cfg.ospf_protocol)
```

No area filtering is needed: the mesh links all live on one protocol
instance, and its interface name prefix (`awg_*`) plus `type == ptp` already
discriminates them from an unrelated second protocol (`werther_v2` on
`eth2`/`eth3`). Remembering a protocol name is simpler and more robust than
remembering an area number, and it needs no per-interface `area` parsing in
rpcd.

### Field derivation

| field | source | notes |
|---|---|---|
| `interface` | `status.ospf[]` iface of `cfg.ospf_protocol`, `type==ptp` | `dummy`/broadcast + unrelated OSPF excluded by protocol/type |
| `to` (peer) | BGP peer protocol name (`peer_<node>_<domain>`) | strip `peer_` + `cfg.bgp_peer_suffix` (`_vehq_ru`) → `node`; match `awg_<token>` ↔ `<token>` |
| `from` | `[main] agent_name` (unchanged) | |
| `source` | `network.interface.<iface>` `ipv6-address[]` | first global addr; no `ip`/shell |
| `target` | far side of the `/127` | flip last address bit of `source` |
| `cost` | new rpcd `set_cost` method | no raw BIRD command in UCI |

Matching note: OSPF neighbor rows give `fe80::` link-locals, not the mesh
global probe target — so `target` is derived from the interface's own `/127`,
never from the OSPF neighbor `ip` field.

## Config shape (after)

```uci
config agent 'main'
	option agent_name 'spoke-1'
	option controller_url 'https://203.0.113.1:9552'
	# ...tls/timeouts/prober knobs unchanged...
	# OSPF scoping: only the links of this OSPF protocol are probed/
	# cost-driven. Chosen by protocol name (simpler than remembering an
	# area number) — an unrelated second OSPF instance is ignored.
	option ospf_protocol 'mesh_v3'
	# Node name = BGP protocol name minus this suffix:
	#   peer_hub_a_example_com minus "_example_com" -> "hub_a"
	option bgp_peer_suffix '_example_com'

# Links are autodiscovered from BIRD. This list un-manages specific ones.
config exclude
	option interface 'dummy_link'     # operator never probes this one
# 	option peer 'peer_legacy_example_com'
```

## Edge cases

- **Multiple OSPF protocols** (e.g. `werther_v2` on `eth2/eth3`): excluded by
  the `ospf_protocol` filter — never discovered, never probed.
- **ptp but no matching BGP peer**: skip + `WARN` (not a managed link).
- **`dummy_awg` / broadcast stubs**: `type != ptp` → excluded by construction;
  also matches "5 ifaces, 4 neighbors" observation on mesh nodes.
- **Neighbor down**: the interface is still listed in `ospf.interfaces[]`
  (config-derived), so it is still probed — desired.
- **No global IPv6 on the interface**: skip + `WARN` (can't derive source/target).
- **Non-`/127` mask**: fall back to reading the peer from the interface route,
  else skip + `WARN`.

## Out of scope / open questions

- **rpcd method name/args**: `set_cost` vs `set_ospf_cost`; whether it takes
  `protocol` or defaults to the single OSPF instance. Decide alongside the
  feed plugin work.
- **`.status` iface `type` field**: exact name (e.g. `type` vs `iftype`)
  fixed when the rpcd change lands; agent follows the real field name.
- **BGP-name ↔ node-name suffix**: `bgp_peer_suffix` required initially;
  revisit once stable.
- **Discovery cadence**: startup + `reload_service` first; periodic refresh
  (pick up added links) as a cheap follow-up.
- **Per-interface overrides** (target/cost) deliberately deferred; `config
  link` remains the escape hatch if ever needed.

## Verification plan

- Extend `tests/` with fixtures of `bird status` JSON (two OSPF protocols,
  ptp + broadcast ifaces, BGP peers) and `network.interface.*` JSON; assert
  the discovered link set equals the reference `{from,to,interface}` triples.
- Manual on a node with two OSPF instances: discovery must yield only
  `mesh_v3`/area ptp links, matching today's hand-written UCI minus `dummy_*`.
- Sanitized throughout: `awg_hub_a`, `peer_hub_a_example_com`, RFC 5737 /
  `fd00::/8` doc ranges.