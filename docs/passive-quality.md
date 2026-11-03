# Passive link quality (design, rev 1)

Status: phase 1 implemented — phase 0 shipped (paced iperf3 TCP leg,
configurable cadences); phase 1 agent half (fw4 include, block task,
`blocks` on the always reply) and controller half (store, join,
classifier, scheduler skip, metrics, dashboard) shipped. Where the
implementation deviates from the text below, the deviation is noted in
place; the rollout check at the end of the work items is still to do.

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
reload (`mielofon-agent/files/usr/share/nftables.d/table-post/mielofon.nft`,
installed under `/usr/share/nftables.d/table-post/`). Everything lives
inside the `inet fw4` table so it shows in `fw4 print`, follows the
firewall's lifecycle and never fights the zone configuration; the agent
writes **no rules**, it only updates a verdict map and set membership and
reads counters.

Shape (see the include for the full text):

- `map mf_colour { type ifname : verdict }` — the egress colour per managed
  interface. The `mielofon_mark` chain (postrouting, `mangle + 10`, after
  fw4's own mangle chain so a QoS rule that sets DSCP runs first) is a
  single `oifname vmap @mf_colour`; the map sends a link to `mielofon_a`
  (OR bit 0 in) or `mielofon_b` (mask it off).
- Eight sets with per-element counters, `mf_{tx,rx}_{a,b}_{s,l}`: the
  element is the interface, the counter that link's packets and bytes for
  the colour, direction and length band (split at 1000 bytes). Every
  managed interface is in all eight, permanently. In the colour chains the
  set lookup selects the tx counter; in `mielofon_count` (prerouting,
  `mangle - 10`, before any zone filtering: a packet fw4 later drops still
  crossed the link) the packet's own colour bit and length select the rx
  counter, and the lookup is the **last** expression so the per-element
  counter only ticks on a full match. Colour-A packets are cleaned so the
  bit never leaves the tunnel.

Why a verdict map rather than moving interfaces between colour sets: a
set element's counter dies with the element, so flipping by membership
would lose the block's tail. With the map, membership is static, counters
persist, and the flip is one atomic `flush map` + `add element`
transaction that no packet ever sees half-done.

Per block the agent issues one assertion transaction — `add element` of
every managed interface to all eight sets (a no-op when present) and the
map rebuild for the colour of the block that begins — and one read,
`nft -j list sets inet` (several `list set` commands in one invocation do
not work: nft builds its cache for the first one only), filtered to the
`mf_*` sets of the table. Two subprocesses per block for the whole node.

Verified on OpenWrt 25.12 with nftables 1.1.6: the include dry-runs clean
inside the fw4 table body; the per-element counter JSON is
`{"elem": {"val": "<iface>", "counter": {"packets": n, "bytes": m}}}`
under the set's `elem`, each set carrying its kernel `handle`.

Interaction notes:

- **Firewall reload** rebuilds the table: counters restart at zero, sets and
  map are empty, and every set gets a new kernel handle. The agent compares
  handles between reads: a change marks the block `reset` (and the next
  block, whose colour's sets are read a block later, the same way) and its
  numbers are not used. The boundary assertion re-adds everything, so the
  agent never tracks reload events; a counter that went backwards without a
  handle change is treated as a reset too.
- **QoS on a tunnel interface.** cake's diffserv tables match exact
  codepoints and would demote marked AF classes to best effort every other
  block. On a tunnel interface classify by tc filter with a masked DSCP match
  or by fwmark, both of which cake honours ahead of its table. nftables QoS
  rules match `dscp & 0x3e`. Nothing on the WAN side sees the inner DSCP.
- **Fragments**: the tunnel MTU is below the LAN MTU, so inner fragmentation
  happens before the mark; each fragment is a packet to both counters.

### Agent

- UCI: `mark_enabled` (default 1), `mark_block_secs` (default 10, 2..600;
  both ends of a tunnel must agree), `mark_table` (default `inet fw4`).
- One block task per node (`mark.rs`). A link's egress colour is `A` for
  even `n` and `B` for odd; at every block boundary the task applies the
  assertion for the block that begins, and half a block later it reads all
  sets and finalises the block that ended for every managed interface:
  - `tx_pkts_s`, `tx_pkts_l`, `tx_bytes` from the egress sets of that
    block's colour (delta since the previous read of those sets, two blocks
    earlier — the other colour's sets are counting the block in progress
    and keep their baseline);
  - `rx_pkts_s`, `rx_pkts_l`, `rx_bytes` from the ingress sets of the same
    colour — what arrived *from the peer* on this interface.
  An interface without a baseline (first read, new link) produces no block
  rather than a zero one.
- Finished blocks queue per interface (bounded, ~10 minutes) and ride on
  the next always-probe reply as the optional `blocks` array. The stream
  model stays "controller opens, agent answers"; the proto change is one
  optional field on `AgentReply::Probe`. Throughput replies never carry
  blocks (they are fenced and may be late).
- A block during which the node's ping gate was paused (a throughput test
  ran) is flagged `under_test`, so the dashboard can tell test-induced loss
  from ambient.
- Metrics: `mielofon_agent_mark_total{result="block|reset|error"}`.
- Failures (include not installed, nft missing) are counted, logged once
  and then once per sixty blocks, and retried forever: a firewall reload
  or a package install fixes them without an agent restart.

```jsonc
// addition to the always-probe reply
"blocks": [ { "n": 174700000, "tx_pkts_s": 812, "tx_pkts_l": 4105, "tx_bytes": 6012345,
              "rx_pkts_s": 790, "rx_pkts_l": 3011, "rx_bytes": 4423110 } ]
// flags, elided when false: "reset": true (counters restarted; store as
// unmeasured), "under_test": true (a throughput test overlapped)
```

### Counting caveats

Lessons from review of the first include; each is a rule the ruleset and
the block task now follow.

- **One packet, one element.** A set lookup ticks its per-element counter
  the moment it matches, whatever the rest of the rule does. The egress
  rules therefore test the protocol explicitly (`meta nfproto`) *before*
  the lookup — left implicit, nft inserts the test right before the
  `ip`/`ip6` statement, after the lookup, and every packet is counted in
  both the IPv4 and the IPv6 rule. The ingress chain counts colour B
  before the colour-A rules, which clean the bit in place and let the
  packet continue; the other order counts every A packet as B too.
- **A baseline is for one block.** The agent's delta for block `n` is
  valid only against the read of block `n − 2`. After a failed or missed
  read the next read of that colour only re-seeds the baseline; a delta
  over two blocks would join with the peer's one-block count as 50 % loss.
- **Under test is an interval, not two instants.** A throughput test with
  its grace can start and end inside one block, between the two moments
  the block task used to sample the gate; the gate now remembers its
  pauses and the block is flagged when any overlapped it.
- **Netfilter counts socket buffers, not wire packets.** With GRO on the
  tunnel interface the receiver's prerouting can see one coalesced buffer
  for several segments the sender's postrouting counted one by one (or
  the other way round for a GSO buffer of a local TCP socket, or a
  forwarded flow GRO coalesced on the LAN side), and `tx − rx` is then
  not loss. The large band is the one affected; the small band (pings,
  DNS, ACKs, QUIC control) is not coalesced. Until measured otherwise the
  rollout check below is the arbiter: if the manual test's retransmits and
  the marking loss disagree on a clean link, turn GRO off on the tunnel
  interfaces (`ethtool -K <iface> gro off`; the wireguard family exposes
  the flag) and GSO/TSO off on them for locally sourced TCP, and compare
  again. The requirement, once known, belongs to the network config, not
  to this package.

### Controller

**Storage.** The counters are appended to the link's time-series as new
optional dimensions on `RawSample`, stamped `ts = n·L`:
`mark_n` (block number), `mark_secs` (L, carried in the block so two ends
on different lengths never join), `mark_tx_s`, `mark_tx_l`, `mark_tx_bytes`,
`mark_rx_s`, `mark_rx_l`, `mark_rx_bytes`, `mark_flags` (bit 0 reset, bit 1
under test). Samples replicate through gossip like every other sample and
`/v1/ts` returns them. Because a block is stamped at its start, behind the
always sample that carried it, the per-link ring is kept sorted by `ts` on
insert (it used to assume monotonic arrival). The joined loss and rate of
a block (`mark_loss_pct`, `mark_rate_mbps`) are written back onto the block
sample by the classifier, and those two are what the 60 s buckets roll up
(`mark_loss`, `mark_rate`).

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
within retention. *As implemented*, the join lives in the classifier pass
(`mark::join`), not in the store's single-link window collapse, since it
needs two links' samples; it runs for every owned link that has a reverse
half whatever `pair_cost` says (`off` only means the halves are costed
separately).

**Window.** `WindowView` gains:

- `mark_loss_pct`: `Σ lost / Σ sent` over the loss window (below),
  `None` unless `Σ sent ≥ mark_min_pkts` (default 2000) and at least one
  joined block exists; a lossy fast window that is itself below the floor
  (a quiet link) falls back to the slow window rather than reporting the
  loss as unmeasured;
- `mark_rate_mbps`: the highest block rate within `tcp_fresh_secs` (the
  throughput freshness horizon, so the scheduler's proof below and the
  rate shown on the map age like a test result);
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
sampling error — only the sample-size floor above. Blocks a throughput test
overlapped (either end's `under_test`) are left out of the loss aggregate
and the counters — a test fills the link and what it drops is its own
congestion — but their rate counts: the bytes crossed.

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
`under-test` (every block of the fast window overlapped a test). A shape
change is logged at `info` and counted in
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
mielofon_link_regime{...,shape="clean|sparse|bursty|size_selective|under_test"} 0|1
mielofon_link_regime_changes_total{...}
mielofon_scheduler_throughput_skipped_total              # tests the proof below saved
```

The counters are kept by the member that joins the link (the one its
agent is registered with) and restart with it; the gauges come from the
replicated derived record like every other per-link gauge.

**Dashboard.** The link detail page gets a loss-versus-rate panel over
`/v1/ts`: per-block rate as area, per-block loss as points, under-test
blocks shaded, the regime chip in the header. The map chip set gains the
regime.

### Scheduling consequences (phase 1 scope)

With passive data available the scheduler skips the gated throughput test
for a link whose window holds a joined block with
`rate ≥ class.good.tcp_mbps` within `tcp_fresh_secs` and
`implied_tcp_mbps` unset or above that threshold, and no carried test
result below it (the classifier keeps scoring a carried `tcp_mbps`, and
only a new test replaces it). Such a link has proven the top class for
free. This is the first byte-saving the passive tier delivers;
phase 2 replaces the test itself.

## Configuration

Agent (`/etc/config/mielofon-agent`):

```text
option mark_enabled '1'
option mark_block_secs '10'
# option mark_table 'inet fw4'
```

The length split (1000 bytes) is a constant of the include, not a knob.

Controller (`[classifier]`):

```toml
mark_window_secs = 300      # fast path: demote on loss within this span
mark_recover_secs = 3600    # slow path: recover after this long clean
mark_min_pkts = 2000        # below this Σ sent the loss is unmeasured
loss_model = "mathis"       # or "mbm"
mss_bytes = 1348
```

## Work items

1. ~~**Agent, nft plumbing**~~ — shipped: include, `nft.rs` (`Nft` trait,
   faked in tests), verdict-map flip, handle-based reset detection, block
   task (`mark.rs`).
2. ~~**Proto**~~ — shipped: `blocks` on `AgentReply::Probe`, round-trip
   tests, `docs/protocol.md`. The controller accepts the field and logs it
   until item 3 lands.
3. ~~**Controller, store**~~ — shipped: block dims on `RawSample`, sorted
   insert, joined loss/rate written back and rolled into the buckets,
   `/v1/ts` exposure; gossip carries them automatically.
4. ~~**Controller, window + classifier**~~ — shipped (`mark.rs`): join,
   asymmetric window, both models, `implied_tcp_mbps` scoring, regime
   label, metrics; unit tests for the missing half, reset blocks, the
   floor, both models against the table above, fast demote / slow recover,
   the shapes, and an end-to-end pass that demotes a link the pings saw
   nothing wrong with.
5. ~~**Scheduler**~~ — shipped: passive proof skips the throughput test for
   the round (`mielofon_scheduler_throughput_skipped_total`).
6. ~~**Frontend**~~ — shipped: per-block loss-versus-rate panel (under-test
   blocks shaded, unjoined blocks as gaps), regime chip, table columns.
7. ~~**Docs**~~ — shipped.

Rollout: enable on one hub pair first and let it run beside the paced
iperf3 for a week; the manual test's retransmit count and the marking loss
over the same minute must agree before the scheduler trusts the passive
proof on every link. Compare per length band: a disagreement confined to
the large band points at GRO/GSO (see *Counting caveats*), not at the
link. The 0.18.0 include counted every sent packet twice and every
colour-A packet twice on ingress; 0.18.1 is the first release whose
counts can be trusted.

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
