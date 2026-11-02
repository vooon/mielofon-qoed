# Passive link quality (design, rev 1)

Status: proposed — phase 0 shipped (paced iperf3 TCP leg, configurable
cadences); phase 1 (this document) not yet implemented.

## Problem

The gated `iperf3` throughput test is the only probe that costs real
traffic, and on metered VPS links it was the whole bill: an unpaced 4 s
upload every 300 s is ~144 GB/day per direction on a gigabit link. Phase 0
cut that two hundred-fold by pacing the TCP leg and probing every 30 min,
but it did not change *what* is measured, and the ping tier cannot replace
the test. Three things make the mesh's links hard to assess cheaply:

1. **A ping stream cannot see a rate policer.** A token-bucket policer on the
   carrier side passes a few kbit/s of ICMP forever; only traffic that
   reaches the policed rate is dropped. Pings say the link is fine while a
   download collapses.
2. **A ping stream cannot resolve sparse loss.** A border DPI that drops one
   packet in a few thousand is a throughput attack on loss-based congestion
   control: for cubic the Mathis bound is `rate ≈ 1.22 × MSS / (RTT × √p)`,
   so on a 60 ms tunnel 0.05 % loss caps a flow at ~10 Mbps and 1 % at
   ~2 Mbps — inside the current `good` loss threshold. Resolving 0.05 % needs
   ~10 000 samples; at 5 pings/s that is over half an hour. Real traffic
   reaches 10 000 packets in about ten seconds at 10 Mbps.
3. **The DPI is stateful and it changes.** A throttle may engage only after a
   flow has carried enough bytes, may key on the source address rather than
   the flow, may drop only full-size packets, and its behaviour is revised
   often. A small test measures the *untriggered* state; a profile measured
   last month is wrong this month.

The underlying mistake was measuring capacity. Operators do not: they know
their capacity and measure production traffic against it. The two standards
this design maps onto are:

- **RFC 9341, Alternate-Marking**: count real packets per colour block at
  both ends of a link; the difference is the real loss of real traffic at the
  real rate, for zero probe bytes.
- **RFC 8337, Model Based Metrics**: never measure bulk capacity; test
  whether a path sustains a *target* rate at a target RTT, with a burst
  pattern derived from the target and a sequential stopping rule, so a
  conclusive pass costs a few megabytes and a failure is cheaper still.

## Principles

- **Measure, do not assume.** The classifier reads live passive state. A
  policer profile is a dated diagnostic snapshot, never a policy input.
- **Passive is primary; active fills gaps.** A link that carries real
  traffic is never tested. Active tests run only for links with no recent
  passive evidence near the class threshold.
- **Conservative model.** Forwarded traffic is mostly cubic; the loss-to-rate
  conversion assumes loss-based congestion control even though the routers'
  own flows run BBR.
- **Asymmetric hysteresis.** Enter a degraded state on one bad aggregate;
  leave it only after a long span of clean blocks at comparable rate. A
  throttle that lifted during idle comes straight back under load.
- **Enough samples or nothing.** A loss estimate from too few packets is
  `null`, never a fabricated 0.
- **Pattern-agnostic primitives.** New DPI behaviour must show up as a new
  shape in the same series and be probed with a new *plan* for the existing
  primitives, not a new release.

## Tiers after phase 1

| Tier | Cost | What it proves | Who runs it |
|------|------|----------------|-------------|
| always (ICMPv6) | ~1 kbit/s | delay, jitter, gross loss, reachability | agent, continuous |
| **passive (alternate marking)** | 0 | real loss at real rate per 10 s block | agent counters, controller join |
| burst test (RFC 8337) — phase 2 | ~5 MB/pass | the link sustains a class target now | controller-dispatched, fenced |
| manual iperf3 | operator's choice | full-rate behaviour incl. triggered throttles | operator via API |
| tcp_info proof — phase 3 | 0 | path lower bound from the routers' own flows | agent sock_diag |

## Passive tier: alternate marking on the tunnel

### Mechanism (RFC 9341 mapped)

- **Colour**: DSCP bit 0 of the *inner* IPv4/IPv6 header. RFC 2474 reserves
  `xxxxx1` codepoints for experimental/local use, so every standard
  CS/AF/EF codepoint has the bit clear: OR-ing it in turns a class into its
  local-use neighbour and masking it off restores it exactly. The one
  collision is Lower Effort (`000001`), which becomes indistinguishable from
  best effort under a masked match; LE is not used on the mesh.
- **Scope**: the mark lives inside the AmneziaWG payload. The carrier never
  sees it (the tunnel copies only ECN to the outer header), so it cannot be
  bleached or trigger anything in a DPI. The only code that can touch it
  between the two counters is the two routers' own rulesets. This is the
  "controlled domain" RFC 9341 requires.
- **Link-based marking**: all traffic on a managed tunnel interface is
  coloured; each hop re-colours, so colours need not agree across links.
- **Blocks**: fixed timer, `L = 10 s`, block number `n = floor(unix_time / L)`
  on each agent's own clock. Both ends need the same `n` for the same
  packets: with clock error `A`, mean one-way delay `D` and delay deviation
  `σ`, the RFC's guard band is `d = A + D + 3σ` and it requires `d < L/2`.
  OpenWrt's `ntpd` keeps `A` in the tens of milliseconds and the tunnels
  are well under 300 ms, so `L = 10 s` has an order of magnitude of margin.
  The finished block `n` is read at `(n+1)·L + L/2`, when its last packets
  have long arrived at the far end.
- **Loss per block**: `lost(A→B, n) = tx(A, n) − rx(B, n)` for the same
  colour. Each agent reports only what it counted; the join happens on the
  controller.
- **Length bands**: counters are split at 1000 bytes. Dropping only
  full-size packets is a known way to kill bulk flows while interactive
  traffic stays clean; the split tells uniform loss from size-selective loss
  and costs two extra counters per colour.

### nftables ruleset

The agent package ships one include that fw4 picks up on every firewall
reload (`/usr/share/nftables.d/table-post/mielofon.nft`, verified present and
empty on OpenWrt 25.12 with nftables 1.1.6). Everything lives inside the
`inet fw4` table so it shows in `fw4 print`, follows the firewall's lifecycle
and never fights the zone configuration; the agent writes **no rules**, it
only moves interface names between sets and reads counters.

```nft
# /usr/share/nftables.d/table-post/mielofon.nft — PLACEHOLDER interface names
# Sets carry per-element counters: the element is the tunnel interface, the
# counter is this link's packet count for that colour and length band.
set mf_tx_a_s { type ifname; counter; }
set mf_tx_a_l { type ifname; counter; }
set mf_tx_b_s { type ifname; counter; }
set mf_tx_b_l { type ifname; counter; }
set mf_rx_a_s { type ifname; counter; }
set mf_rx_a_l { type ifname; counter; }
set mf_rx_b_s { type ifname; counter; }
set mf_rx_b_l { type ifname; counter; }

# Egress: just before encapsulation, after fw4's own mangle_postrouting so a
# QoS rule that sets DSCP runs first. A link's interface is in exactly one
# colour's pair of sets (s and l); the agent flips it every L seconds.
chain mielofon_mark {
  type filter hook postrouting priority mangle + 10; policy accept;
  meta length <= 1000 oifname @mf_tx_a_s ip6 dscp set ip6 dscp | 0x01
  meta length <= 1000 oifname @mf_tx_a_s ip  dscp set ip  dscp | 0x01
  meta length >  1000 oifname @mf_tx_a_l ip6 dscp set ip6 dscp | 0x01
  meta length >  1000 oifname @mf_tx_a_l ip  dscp set ip  dscp | 0x01
  meta length <= 1000 oifname @mf_tx_b_s ip6 dscp set ip6 dscp & 0x3e
  meta length <= 1000 oifname @mf_tx_b_s ip  dscp set ip  dscp & 0x3e
  meta length >  1000 oifname @mf_tx_b_l ip6 dscp set ip6 dscp & 0x3e
  meta length >  1000 oifname @mf_tx_b_l ip  dscp set ip  dscp & 0x3e
}

# Ingress: right after decapsulation, before fw4's mangle_prerouting and any
# zone filtering (a packet fw4 later drops still crossed the link). Every
# managed interface is in all four rx sets; the colour is the packet's. The
# set lookup is the LAST expression so its per-element counter only ticks
# when the colour and length matched. Colour A packets are cleaned so the
# bit never leaves the tunnel.
chain mielofon_count {
  type filter hook prerouting priority mangle - 10; policy accept;
  ip6 dscp & 0x01 == 0x01 meta length <= 1000 iifname @mf_rx_a_s ip6 dscp set ip6 dscp & 0x3e
  ip6 dscp & 0x01 == 0x01 meta length >  1000 iifname @mf_rx_a_l ip6 dscp set ip6 dscp & 0x3e
  ip6 dscp & 0x01 == 0x00 meta length <= 1000 iifname @mf_rx_b_s
  ip6 dscp & 0x01 == 0x00 meta length >  1000 iifname @mf_rx_b_l
  ip  dscp & 0x01 == 0x01 meta length <= 1000 iifname @mf_rx_a_s ip  dscp set ip  dscp & 0x3e
  ip  dscp & 0x01 == 0x01 meta length >  1000 iifname @mf_rx_a_l ip  dscp set ip  dscp & 0x3e
  ip  dscp & 0x01 == 0x00 meta length <= 1000 iifname @mf_rx_b_s
  ip  dscp & 0x01 == 0x00 meta length >  1000 iifname @mf_rx_b_l
}
```

The whole ruleset dry-runs clean (`nft -c`) on the target nftables; the
only runtime behaviour still to confirm on a node is the JSON shape of
`list set` with per-element counters (phase 1 task 1).

Per link the agent issues, in one `nft -f -` transaction every block:

```nft
delete element inet fw4 mf_tx_a_s { "awg_hub_a" }
delete element inet fw4 mf_tx_a_l { "awg_hub_a" }
add    element inet fw4 mf_tx_b_s { "awg_hub_a" }
add    element inet fw4 mf_tx_b_l { "awg_hub_a" }
```

and reads all eight sets with one `nft -j -f -` fed the eight `list set`
lines. Two subprocesses per block for the whole node, the same low-rate CLI
pattern as the ubus calls.

Interaction notes:

- **Firewall reload** rebuilds the table: counters restart at zero and every
  set is empty. The agent sees a counter go backwards (or an element gone),
  discards that block and the next one, and re-asserts membership. Adding
  an element that exists is a no-op, so the agent simply asserts its intended
  state every block and never tracks reload events.
- **QoS on a tunnel interface.** cake's diffserv tables match exact
  codepoints and would demote marked AF classes to best effort every other
  block. On a tunnel interface classify by tc filter with a masked DSCP match
  or by fwmark, both of which cake honours ahead of its table. nftables QoS
  rules match `dscp & 0x3e`. Nothing on the WAN side sees the inner DSCP.
- **Fragments**: the tunnel MTU is below the LAN MTU, so inner fragmentation
  happens before the mark; each fragment is a packet to both counters.

### Agent

- `mark_enabled` (default 1), `mark_block_secs` (default 10, min 2).
- One block task per node (not per link). A link's egress colour is `A`
  for even `n` and `B` for odd `n`; the task switches every managed link at
  the block boundary, and at `L/2` past the boundary it reads all eight
  sets and finalises block `n−1` for every link, both directions it can
  see:
  - `tx_pkts_s`, `tx_pkts_l`, `tx_bytes` from the egress sets of the colour
    block `n−1` had;
  - `rx_pkts_s`, `rx_pkts_l`, `rx_bytes` from the ingress sets of the same
    colour — what arrived *from the peer* on this interface.
  Byte counts come from the same per-element counters.
- Finished blocks ride on the next always-probe reply as an optional
  `blocks` array (0–2 entries at the 15 s cadence; up to a bounded backlog
  when a reply was missed). The stream model stays "controller opens, agent
  answers"; the proto change is one optional field on `AgentReply::Probe`.
- A block overlapping a throughput test the agent ran is flagged
  `under_test`, so the dashboard can tell test-induced loss from ambient.

```jsonc
// addition to the always-probe reply
"blocks": [ { "n": 174700000, "tx_pkts_s": 812, "tx_pkts_l": 4105, "tx_bytes": 6012345,
              "rx_pkts_s": 790, "rx_pkts_l": 3011, "rx_bytes": 4423110,
              "reset": false, "under_test": false } ]
```

`reset: true` marks a block the agent could not trust (counters restarted);
the controller stores it as unmeasured.

### Controller

**Storage.** The counters are appended to the link's time-series as new
optional dimensions on `RawSample`, stamped `ts = n·L`:
`blk` (block number), `tx_pkts_s`, `tx_pkts_l`, `tx_bytes`, `rx_pkts_s`,
`rx_pkts_l`, `rx_bytes`, `under_test`. Nothing else changes: samples
replicate through gossip like every other sample, the 60 s buckets roll them
up, `/v1/ts` returns them.

**Join.** Loss is directional and needs both ends: `tx` of block `n` from the
owner of `A→B` and `rx` of block `n` reported by the owner of `B→A` (its
ingress counters). `pair_links` already maps a link to its reverse half and
the reverse half's samples are available through replication, so the join is
one lookup per block in the store's window collapse:

```text
sent(A→B, n)   = tx_pkts_s + tx_pkts_l           from A→B's sample at n
received(A→B,n)= rx_pkts_s + rx_pkts_l           from B→A's sample at n
lost(A→B, n)   = max(sent − received, 0)
rate(A→B, n)   = tx_bytes × 8 / L
```

A block with only one half present (the peer's agent is registered at a
member whose samples have not arrived yet, or is down) is skipped, not
guessed; the join is retried on every classifier pass while both samples are
within retention.

**Window.** `WindowView` gains:

- `mark_loss_pct`: `Σ lost / Σ sent` over the loss window (below),
  `None` unless `Σ sent ≥ mark_min_pkts` (default 2000) and at least one
  joined block exists;
- `mark_rate_mbps`: the highest block rate in the window;
- `mark_loss_l_pct` / `mark_loss_s_pct`: the same per length band;
- `implied_tcp_mbps`: the TCP rate the measured loss supports at the link's
  current `rtt_ms` (see model), `None` when loss is unmeasured. A measured
  loss of exactly zero yields `None` rather than infinity: no loss is no
  constraint.

The **loss window is asymmetric**: when the most recent
`mark_window_secs` (default 300) show any loss, the aggregate is taken over
that short window so the demotion is fast; when they are clean, the aggregate
is taken over `mark_recover_secs` (default 3600), so a single bad block
inside the last hour still counts and the class recovers only after an hour
of clean blocks. Because every block is an exact count, the aggregate has no
sampling error — only the sample-size floor above.

**Model.** `implied_tcp_mbps` converts loss to rate under loss-based
congestion control. Two forms, selected by `[classifier] loss_model`:

- `mathis` (default): `rate = 1.22 × MSS × 8 / (RTT × √p)`; the long-run
  Reno/cubic bound.
- `mbm`: RFC 8337's run length. The allowed loss for a target is
  `p ≤ 1 / (3 × W²)` with `W = ceil(target × RTT / (MSS × 8))`; inverting
  it gives the largest target the measured `p` still allows. About three
  times stricter than Mathis.

`mss_bytes` defaults to 1348 (1420-byte tunnel MTU minus IPv6+TCP headers).
RTT is the window's `rtt_ms`; with no fresh RTT the implied rate is `None`.
Worked values for the mesh's ~60 ms tunnels under `mathis`:

| loss | implied TCP rate | class by `tcp_mbps` default thresholds |
|------|------------------|----------------------------------------|
| 0.01 % | ~30 Mbps | good |
| 0.05 % | ~10 Mbps | good (at the bound) |
| 0.5 % | ~3 Mbps | poor |
| 1 % | ~2 Mbps | poor/bad |

**Classifier.** `implied_tcp_mbps` is scored as a lower-bound dimension
against the same per-class `tcp_mbps` thresholds the test uses (worst crossed
class wins, like every other dimension). Nothing in the class table changes;
`loss_pct` from pings keeps its coarse thresholds as the gross-outage signal.
Busy hold and pair merging apply unchanged; in `directional` pair mode the
implied rate is the direction's own, in `symmetric` the worse of both.

**Regime.** On every classifier pass the controller labels the link's recent
block series with a loss *shape*, kept in the derived record and shown as a
chip on the map: `clean`, `sparse` (loss in most blocks, max block loss
≤ 3× mean), `bursty` (loss concentrated in few blocks), `size-selective`
(large-packet loss ≥ 5× small-packet loss with both bands sampled),
`under-test`. A shape change is logged at `notice` and counted in
`mielofon_link_regime_changes_total`. This is the early warning that the DPI
moved; it drives nothing automatically.

**Metrics.** New per-link series on `/metrics`, meant for the long-retention
store outside the controller (the in-memory history keeps a day):

```text
mielofon_link_mark_loss_pct{from,to,interface}
mielofon_link_mark_rate_mbps{...}
mielofon_link_implied_tcp_mbps{...}
mielofon_link_mark_sent_total{...,band="s|l"}     # counters, monotonic
mielofon_link_mark_lost_total{...,band="s|l"}
mielofon_link_regime{...,shape="clean|sparse|bursty|size_selective"} 0|1
mielofon_link_regime_changes_total{...}
```

**Dashboard.** The link detail page gets a loss-versus-rate panel over
`/v1/ts`: per-block rate as area, per-block loss as points, under-test
blocks shaded, the regime chip in the header. The map chip set gains the
regime.

### Scheduling consequences (phase 1 scope)

With passive data available the scheduler skips the gated throughput test
for a link whose window holds a joined block with
`rate ≥ class.good.tcp_mbps` within `tcp_fresh_secs` and
`implied_tcp_mbps` unset or above that threshold. Such a link has proven the
top class for free. This is the first byte-saving the passive tier delivers;
phase 2 replaces the test itself.

## Configuration

Agent (`/etc/config/mielofon-agent`, defaults in parentheses):

```text
option mark_enabled '1'
option mark_block_secs '10'
option mark_length_split '1000'   # bytes; short vs long band
```

Controller (`[classifier]`):

```toml
mark_window_secs = 300      # fast path: demote on loss within this span
mark_recover_secs = 3600    # slow path: recover after this long clean
mark_min_pkts = 2000        # below this Σ sent the loss is unmeasured
loss_model = "mathis"       # or "mbm"
mss_bytes = 1348
```

## Work items

1. **Agent, nft plumbing** — ship the include; `nft` wrapper beside `bus.rs`
   (`Nft` trait, faked in tests); confirm the `list set` JSON shape on a
   25.12 node; set-membership assert and reset detection; block task.
2. **Proto** — `blocks` on `AgentReply::Probe`; round-trip test; `docs/protocol.md`.
3. **Controller, store** — new `RawSample` dims; bucket rollup; `/v1/ts`
   exposure; gossip carries them automatically.
4. **Controller, window + classifier** — join, asymmetric window, model,
   `implied_tcp_mbps` scoring, regime label, metrics. Unit tests: join with
   a missing half, reset blocks, sample-size floor, both models against the
   table above, fast demote / slow recover, regime shapes.
5. **Scheduler** — passive proof skips the throughput test.
6. **Frontend** — panel and chips; `logic.test.js` for the series shaping.
7. **Docs** — architecture, protocol, UCI and TOML samples; this document
   moves to *implemented*.

Rollout: enable on one hub pair first and let it run beside the paced
iperf3 for a week; the manual test's retransmit count and the marking loss
over the same minute must agree before the scheduler trusts the passive
proof on every link.

## Later phases (summary)

- **Phase 2 — in-agent burst test.** A UDP sender/receiver in the agent
  running RFC 8337's sustained full-rate burst test at a class target
  (`target_window_size` packets every `target_RTT`, SPRT stopping rule;
  ~5 MB for a conclusive `good` at 10 Mbps / 40 ms), dispatched over the
  existing command streams with the fence, result mapped to `tcp_mbps` as
  the highest passing target. The existing throughput tier stays as a
  manual, API-triggered run with a *characterise* mode (1 s blocks, iperf3
  intervals retained, pings kept running and flagged) and an experiment
  runner that executes operator-submitted plans of send-pattern primitives
  — the bucket drain and the refill/trigger/decay bisections are plans, not
  code. iperf3 remains a feed dependency until the burst test has run for a
  while.
- **Phase 3 — tcp_info proof.** Delivery rate and the app-limited flag over
  sock_diag for the routers' own mesh-bound sockets; the controller credits
  every link on the flow's path with the non-app-limited maximum as a lower
  bound. Proof only, never a demotion.
- **Phase 4 — polish.** Always-tier padding towards the tunnel MTU for
  path-MTU black holes; firewall-zone names as an agent configuration
  convenience over autodiscovery.

## Explicitly not done

- No proxying or connection splitting of forwarded traffic, no reliable or
  FEC tunnel transport: mielofon measures and steers, the transport stays
  AmneziaWG.
- No eBPF. Alternate marking makes per-flow retransmission tracking
  redundant for links with an agent on both ends, and it is blind to QUIC
  anyway. The one case it would serve — a far end that is not ours — does
  not exist on the mesh.
- No policer profile as classifier input, and no remembered trigger
  thresholds in the scheduler: the burst test stays small by construction
  and the counters run during it.
