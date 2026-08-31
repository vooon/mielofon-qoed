# Handoff: Agent Build Spec — `mielofon`

**Project title:** Mielofon — distributed QoE coordination for the AmneziaWG mesh (controller + agent).

**Repo (PUBLIC):** `github.com/<you>/mielofon`

---

## ⚠️ SANITIZATION MANDATE — READ FIRST

You are building a **publicly accessible** repository. The requester's production network is **private**. Under no circumstances may the repo, README, code, comments, docs, CI, tests, examples, or commit messages expose:

- **Real IP addresses** of any node (public or private, IPv4 or IPv6).
- **Real hostnames / hub / spoke / site names** (never use real ones — use only the placeholders `hub-a`..`hub-e` / `node-0..4` listed below).
- **Real OOB / management addressing** (no nebula addresses, no mesh link-local/ULA scheme).
- Any real ASN, `net_id`, domain, DNS service, or topology layout that maps to production.

Use only **placeholder tokens and RFC-private documentation ranges**:
- Placeholder node labels: `hub-a`, `hub-b`, `hub-c`, `hub-d`, `hub-e` (or `node-0..4`).
- Documentation/host IPs: `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24` (RFC 5737), `2001:db8::/32` (RFC 3849), ULA `fd00::/8` for private-space examples.
- ASNs: use `64512..65534` (private) placeholders only.
- Do NOT describe the real mesh's addressing scheme as "how it is" in production; describe it generically ("the mesh runs AMW / point-to-point /127 links and loopbacks" without the real prefix).

Treat any config, example, or doc as if a stranger will run it identically against their own unrelated infrastructure.

---

## 1. Project purpose

A distributed, eventually-consistent coordination layer that helps a mesh of routers (running OSPF on AmneziaWG point-to-point links) **steer traffic to good links and away from degraded ones**, based on live link quality, and gives an operator a **path/dashboard view** of why traffic goes where it goes.

Two artifacts shipped together:
- **`mielofon-controller`** — a small daemon (Rust) forming a multi-node cluster. Holds a replicated per-link quality store, a probe mutual-exclusion fence, an mTLS HTTP API, and a dashboard.
- **`mielofon-agent`** — a lightweight **ucode** client that runs on each mesh router: probes its own outgoing links, reports measurements, and applies an OSPF cost policy back to the local router.

The requester's Ansible integration lives **in a separate private repo**; you are not responsible for it. This repo is the controller source + agent source + docs + build of the OpenWrt packages.

---

## 2. Components

### 2.1 `mielofon-controller` (Rust daemon)
- Single static binary: build target **`x86_64-unknown-linux-musl`** (no runtime/SSL lib dependency).
- Runs on **5 nodes** (all are voters). The fabric between nodes is the **operator WAN/public network** — NOT the mesh underlay — so coordination traffic and probing both stay off the links being measured, and a mesh underlay flapping doesn't kill the cluster.
- **Consistency:** **eventual** via **gossip / anti-entropy**. A last-write-wins (LWW) or CRDT-style approach for the replicated per-link key/value store is acceptable; full linearizable consensus is **not required**.
- **Probe fence:** a **soft lease**-based mutex so only one agent is running the intrusive throughput probe cluster-wide. Because consistency is eventual, a rare overlapping probe is tolerable — mark the affected sample `conflict`.
- **mTLS security:** all node↔node and node↔agent traffic is **mutual TLS** using a dedicated CA; `rustls` preferred (pure Rust, static-friendly). Split the listener into two ports or two listeners: one for **cluster members**, one for **agent clients** (so ACLs are clean). No anonymous public endpoints.
- **Dashboard:** embed a small static web UI served from each node over the mTLS (or a separately-gated read endpoint). Show per-link live metrics, quality class, applied cost, `state: quiet|busy|conflict`, and a **path view** (route from source to destination composed of per-link segments with live quality).

### 2.2 `mielofon-agent` (ucode)
- Pure **ucode** (the OpenWrt scripting language), no Python.
- Runs on every mesh router. For each configured outgoing link, on a schedule:
  1. **Always-on tier (never gated):** RTT (ping), loss, and a transaction-rate probe (netperf `TCP_RR`/`UDP_RR`). These are cheap and **congestion-immune** — they stay valid even while real user traffic (e.g. a large download) is flowing.
  2. **Throughput tier (gated):** a bulk-throughput probe (netperf `TCP_STREAM` / `UDP_STREAM` or iperf3). Only run when:
     - the global **lease/fence** is held (no other agent is running a throughput probe), **and**
     - the link's instantaneous utilization is **below a quiet threshold** (`QUIET_MAX`).
     If the link is busy, record `{state: busy, util_mbps}` and **skip the throughput probe** (never report it as "degraded").
- Applies an **OSPF cost policy** back to the local router (control-plane mechanism is out-of-scope/injected by the private repo, but the agent should expose a clean hook/interface for "apply cost per link").
- ucode modules used: `utoop`/`uloop`, `uci`, `socket`, `fs`, `math`, `log`. For running external binaries (ping, netperf, iperf3) prefer short-lived `uloop.process`/`fs.exec` calls.

---

## 3. Repo layout

The repo is **both the source and an OpenWrt feed**: `crates/`, `agent/`, `docs/` hold
the source; the top-level `mielofon-controller/` and `mielofon-agent/` dirs are feed
packages. OpenWrt's feed scanner walks the whole repo for any Makefile defining a
`Package/`, so the non-package dirs are ignored by the feed. `feeds.conf`:
`src-git vooon https://github.com/vooon/mielofon.git` (drop directly into the feed).

```
mielofon/
├─ Cargo.toml
├─ crates/
│  └─ controller/            # Rust daemon (cluster, gossip, KV, fence, mTLS API, dashboard)
│     └─ src/
│        ├─ main.rs
│        ├─ config.rs        # config file + CLI
│        ├─ gossip.rs        # anti-entropy / state replication
│        ├─ kv.rs            # per-link LWW store
│        ├─ fence.rs         # soft lease
│        ├─ api.rs           # HTTP mTLS handlers (members + clients)
│        └─ dashboard/       # embedded static UI (or shared assets)
├─ agent/
│  └─ ucode/
│     ├─ mielofon-agent.uc   # main daemon
│     ├─ probe.link.uc       # per-link probe (always-on + gated)
│     ├─ apply.cost.uc       # generic "apply OSPF cost per link" (stub/hook)
│     └─ config.example      # /etc/config/mielofon-agent example (sanitized)
├─ mielofon-controller/      # feed pkg Makefile: prebuilt static musl binary
│  └─ Makefile
├─ mielofon-agent/           # feed pkg Makefile + files/ (init, config) — PKGARCH=all
│  ├─ Makefile
│  └─ files/
│     ├─ mielofon-agent.init # procd init script
│     └─ mielofon-agent.conf # /etc/config/mielofon-agent sample
├─ docs/
│  ├─ architecture.md        # full design (SANITIZED)
│  ├─ protocol.md            # HTTP/JSON API + gossip + fence semantics
│  ├─ probe.md               # tiers, quiet gate, parameters
│  └─ CONTRIBUTING.md
├─ .github/workflows/        # CI: cargo fmt/clippy/test, build static x86_64-musl artifact
└─ README.md
```

Feed conventions (mirror `vooon/my-openwrt-feed`): `include $(TOPDIR)/rules.mk` +
`include $(INCLUDE_DIR)/package.mk`; `Package/<pkg>/install` + `/conffiles`; agent is
`PKGARCH:=all` with ucode deps (`ucode-mod-uci`, `ucode-mod-fs`, `ucode-mod-uloop`...).
Use `rpcd-mod-bird` for `birdc` via ubus where the apply-cost hook needs it.

---

## 4. Wire/protocol sketch (definitive, to be detailed in `docs/protocol.md`)

### API surface (mTLS)
- `POST /v1/fence/acquire {agent, link}` → `{ok, token, ttl}` or `{ok:false, reason:busy|held}`
- `POST /v1/fence/release {token}`
- `POST /v1/quality {link, ts, rtt_ms, loss_pct, rr_tps, tcp_mbps, udp_mbps, util_mbps, state}` → `{accepted}`
- `GET /v1/quality?link=` → latest per-link record
- `GET /v1/quality/all` → full replicated view (feeds dashboard)
- `GET /v1/policy?link=` → current per-link OSPF cost decision
- `GET /v1/status` → node/cluster/fence state
- Members listener additionally: internal gossip/anti-entropy calls.

### KV record (logical, expires/LWW)
```
link: {node_from, node_to, interface}
ts
rtt_ms, loss_pct, rr_tps
tcp_mbps, udp_mbps
util_mbps
state: quiet | busy | conflict
quality: good | acceptable | poor | bad   (classification on controller)
ospf_cost
```
- Controller assigns `quality`/`ospf_cost` from the reported metrics (configurable thresholds; keep conservative defaults; do not invent a "dead/broken" cost — let the underlay's dead detection own true outages).

### Fence semantics
- Soft lease with TTL and renewal. On expiry without release, another agent may take over. Overlap is ok; mark `conflict`.
- Only the **throughput tier** uses the fence. The always-on tier (RTT/loss/RR) does **not** need the fence.

### Probe quiet gate
- Agent samples instantaneous link utilization (derive from byte counters over a short window) before a throughput probe.
- If `util > QUIET_MAX` → skip throughput, record `state: busy`, `util_mbps`.
- `QUIET_MAX` default ~15 Mbps, configurable.

---

## 5. Security (mTLS) — required

- Dedicated CA for the cluster. Issue a **server+client cert per node** and a **client cert per agent**.
- All peers verified against the pinned CA; also verify host identity / SAN.
- Use `rustls` (pure-Rust) to stay static-friendly and avoid OpenSSL coupling.
- No anonymous endpoints. Dashboard over mTLS only; optionally layer basic-auth/allowlist.
- CA private key lives on the **operator's control plane**, not on any cluster node (inject at deploy). Do not commit any private key, cert, or CA to this public repo. Ship only the key/cert *generation scheme* / sample generation script with clearly marked placeholders.

---

## 6. Configuration (sanitized examples)

Config is deliberately generic; all identifiers are placeholders the operator rewrites for their environment.

```toml
# contribution: mielofon-controller.toml (example, SANITIZED)
[node]
name = "hub-a"          # placeholder — do not use real hostnames
advertise = "203.0.113.1"   # RFC5737 documentation address — replace in deployment

[cluster.fabric]
transport = "tcp-mtls"
port = 9443

[members]               # all voters
"hub-a" = "203.0.113.1"   # documentation range only
"hub-b" = "203.0.113.2"
"hub-c" = "203.0.113.3"
"hub-d" = "203.0.113.4"
"hub-e" = "198.51.100.9"

[clients]
port = 9444

[tls]
ca = "/etc/mielofon/ca.pem"
cert = "/etc/mielofon/node.pem"
key = "/etc/mielofon/node.key"

[quality.good]
rtt_ms = 40
loss_pct = 1.0
rr_tps = 50.0
tcp_mbps = 10.0
ospf_cost = 10

[quality.acceptable]
rtt_ms = 90
loss_pct = 2.5
rr_tps = 35.0
tcp_mbps = 5.0
ospf_cost = 20

[quality.poor]
rtt_ms = 250
loss_pct = 5.0
rr_tps = 20.0
tcp_mbps = 2.0
ospf_cost = 50

[quality.bad]
rtt_ms = 500
loss_pct = 10.0
rr_tps = 10.0
tcp_mbps = 1.0
ospf_cost = 100
```

Agent config (`/etc/config/mielofon-agent` example) uses the same placeholder pattern.

---

## 7. Build / packaging

- **Controller build:** `cargo build --release --target x86_64-unknown-linux-musl`. Provide CI (`.github/workflows`) that lints (`fmt`, `clippy`), runs tests (`cargo test`), and **uploads the static binary as a release artifact**.
- **This repo is the OpenWrt feed.** It ships both `Package/` definitions (top-level `mielofon-controller/`, `mielofon-agent/`), so it can be added to `feeds.conf` directly (e.g. `src-git vooon https://github.com/vooon/mielofon.git`) — there is no separate feed repo for these.
  - `mielofon-controller` — installs the **static binary** (prebuilt release artifact referenced via PKG_SOURCE) to `/usr/sbin/` + config + init.
  - `mielofon-agent` — `PKGARCH=all`, ucode deps (`ucode-mod-fs`, `ucode-mod-uloop`, `ucode-mod-socket`, etc.), installs `.uc` scripts + init + `/etc/config/mielofon-agent`.
  - Provide both `Package/.../install` blocks and a `conffiles` list.
- Ensure the **static binary** is truly static (`ldd` shows no dynamic deps) — the musl target gives this.

---

## 8. Acceptance criteria

- `cargo test`, `fmt`, `clippy` clean in CI.
- One static `x86_64-unknown-linux-musl` binary per `mielofon-controller` release.
- OpenWrt `Makefile` for both packages builds in the **requester's feed** (you provide files; requester integrates).
- Agent runs on OpenWrt with **only ucode deps** (no Python), probes both tiers, honors the quiet gate, and reports `state: busy|conflict` correctly.
- Gossip KV converges; a node joining/leaving the 5-node cluster is handled.
- mTLS enforced on every endpoint; no anonymous access.
- **Sanitization:** full repo review finds zero real IPs/hostnames/OOB/DNS/ASN references. Use only placeholders + RFC-private ranges.

---

## 9. Non-goals / out of scope (for this agent)

- Real-owned Ansible roles/playbooks (separate private repo).
- The actual OSPF/BIRD configuration of the production mesh.
- Long-term metric archival (out-of-scope; the controller's live store + dashboard is the target; a scraper can be added later).
- Automating real cert distribution (provide the scheme/scripts with placeholders only).

---

## 10. Suggested implementation order

1. `crates/controller`: config + node identity + mTLS TLS setup.
2. Gossip / anti-entropy KV (LWW) + node membership.
3. HTTP API (members + clients) over mTLS.
4. Fence (soft lease) + `POST /v1/fence/*`.
5. `agent/ucode`: always-on probes; then utilization gate + throughput tier; then fence integration; then apply-cost hook.
6. Dashboard (embed) + `/v1/quality/all` + `GET /v1/graph` path view.
7. OpenWrt `Makefile` for both packages; CI static build.
8. Docs (`architecture`, `protocol`, `probe`), README, and a **sanitization pass** (grep for any literal `[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+` and real identifiers; replace with placeholders).
