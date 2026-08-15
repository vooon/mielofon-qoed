# Architecture

Mielofon is a distributed QoE (link-quality) coordination layer for an OSPF
mesh running over AmneziaWG point-to-point links. It steers traffic to good
links and away from degraded ones, and gives an operator a per-link and path
view of why traffic goes where it goes.

This document describes the controller (`crates/controller`). The agent is
described separately.

> **Sanitization**: this is a public repo describing software for a private
> mesh. Only placeholder node names (`hub-a`..`hub-e`) and RFC-private
> documentation addresses are used.

## Repository

The repo is both the mielofon source **and** an OpenWrt feed. In `feeds.conf`:

```
src-git mielofon https://github.com/vooon/mielofon-qoed.git
```

- `crates/` — Rust workspace source (`controller`, `agent`, `mielofon-otel`).
- `mielofon-controller/` and `mielofon-agent/` — feed `Package/` definitions.
- OpenWrt's feed scanner ignores non-package directories (e.g. `crates/`).

## Controller

A small asynchronous Rust daemon (tokio + axum + rustls) forming a multi-node
cluster. Runs on a small set of nodes (all voters). The fabric between nodes is
the operator WAN/public network, **not** the mesh underlay, so coordination and
probing stay off the links being measured.

### Consistency

**Eventual** via gossip / anti-entropy with a last-write-wins (LWW) per-link
store. Full linearizable consensus is not required. A node joining/leaving is
handled by membership in the config; a lost node's records age out via the
store's expiry (`grace_ttl_secs`).

### Listeners

| Listener | Port | Transport | Purpose |
|----------|------|-----------|---------|
| members  | 9551 | mTLS (rustls) | cluster gossip / anti-entropy |
| clients  | 9552 | mTLS (rustls) | agents: quality reports + probe fence |
| admin    | 9553 | HTTP on loopback | dashboard `/`, `/metrics`, `/healthz`, `/readyz`, read endpoints |

The admin listener binds `127.0.0.1` and is intended to be exposed through a
reverse proxy (e.g. HAProxy). It is **not** mTLS; the proxy is the exposure
boundary.

### Probe fence (soft lease)

Only the intrusive throughput probe tier uses the fence. An agent acquires a
short-lived lease before running a gated throughput probe; only one agent does
so cluster-wide. Because consistency is eventual, a rare overlap is tolerated —
the agent reports it as `conflict` rather than degraded. See `protocol.md`.

### Quality classification

The controller maps reported metrics to `good` / `acceptable` / `poor` / `bad`
using conservative thresholds, and derives a per-link OSPF cost. The controller
**never** writes a dead/broken state — hard outages are owned by the underlay's
dead-interval so the no-controller state is safe. Importantly, a low-RTT link
that is throughput-throttled is still penalised (the key failure mode active
probes must catch).

### Lockdown rules

- mTLS on members + clients, verified against a dedicated CA; no anonymous
  endpoints.
- The CA private key lives on the operator's control plane, not on any node.
- The admin listener is loopback-only plus whatever the proxy is configured to
  expose.

## Observability

Two paths coexist on the admin listener / via OTLP:

- **Prometheus scrape**: `/metrics` on the admin listener serves per-link gauges
  and a reports counter in text exposition format.
- **OpenTelemetry, OTLP/HTTP (gRPC-free)**: traces, logs, and metrics are
  exported to a collector configured under `[otel]`. The
  `opentelemetry-otlp` `grpc-tonic` feature is deliberately not enabled, so no
  gRPC is pulled into the static binary. Configuration mirrors the shape used
  by `pathosd`.

See `crates/mielofon-otel` for the shared telemetry setup.

## Non-goals

- Long-term metric archival (the live store + dashboard is the target).
- Automating real certificate distribution.

## Layout

```
crates/controller/src/
  main.rs        entrypoint: three listeners, background gossip + prune
  config.rs      TOML config (node, members, listeners, tls, quality, otel)
  tls.rs         rustls mTLS (server + client) built from a pinned CA
  state.rs       shared AppState
  kv.rs          LWW per-link store
  model.rs       Link, QualityRecord, Quality, ProbeState
  fence.rs       soft-lease probe mutex
  quality.rs     classification + OSPF cost derivation
  api.rs         axum handlers and routers for the three listeners
  gossip.rs      anti-entropy exchange + periodic push loop
  remote.rs      minimal mTLS HTTPS client for gossip pushes
  dashboard.rs   embedded static dashboard
```