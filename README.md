# Mielofon

Mielofon is a distributed QoE coordination layer for an OSPF mesh running over
AmneziaWG point-to-point links. It steers traffic toward good links and away
from degraded ones, based on live link quality, and gives operators a view of
why traffic goes where it goes.

This repository is **both the source and an OpenWrt feed**:

- `crates/controller` — the controller daemon (Rust)
- `crates/agent` — the router agent (Rust)
- `crates/mielofon-otel` — shared OpenTelemetry (OTLP/HTTP, gRPC-free) setup
- `mielofon-controller/`, `mielofon-agent/` — feed `Package/` definitions

## Status

Stage 1 (controller) is implemented and validated end-to-end (mTLS listeners,
fence, LWW KV, quality classification, gossip, dashboard, `/metrics`,
`/healthz`, `/readyz`, OTEL). Stage 2 (agent) is in progress.

## Feed installation

Add to `feeds.conf`:

```
src-git mielofon https://github.com/vooon/mielofon-qoed.git
```

Then:

```sh
./scripts/feeds update -a
./scripts/feeds install -a
```

Select `mielofon-controller` / `mielofon-agent` under **Network** in menuconfig.

## Build & verify

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — design and listeners
- [`docs/protocol.md`](docs/protocol.md) — API, fence, gossip wire format
- [`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md) — sanitization + contribution
  guide

> **Sanitization**: this public repo uses only placeholders (`hub-a`..`hub-e`)
> and RFC-private documentation addresses.