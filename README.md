# Mielofon

Mielofon is a distributed QoE coordination layer for an OSPF mesh running over
AmneziaWG point-to-point links. It steers traffic toward good links and away
from degraded ones, based on live link quality, and gives operators a view of
why traffic goes where it goes.

This repository is **both the source and an OpenWrt feed**:

- `crates/controller` — the controller daemon (Rust) + a `cert` subcommand for
  mTLS certificate generation
- `crates/mielofon-otel` — shared OpenTelemetry (OTLP/HTTP, gRPC-free) setup
- `mielofon-agent/` — the router agent (ucode)
- `mielofon-controller/`, `mielofon-agent/` — feed `Package/` definitions

## Status

Stage 1 (controller) and Stage 2 (ucode agent) are implemented and validated
end-to-end (mTLS listeners, long-poll command/reply, fence, LWW KV, quality
classification, gossip, dashboard, `/metrics`, `/healthz`, `/readyz`, OTEL,
node-exporter textfile metrics on the agent).

## TLS certificates

Generate the mTLS CA and per-node/per-agent certificates on the operator's
control plane (a host with `openssl`):

```sh
mielofon-controller cert ca --name mielofon-ca
mielofon-controller cert node --name hub-a --ip 203.0.113.1 --host hub-a --ca-key ca.key --ca-crt ca.crt
mielofon-controller cert agent --name spoke-1 --ca-key ca.key --ca-crt ca.crt
```

Place the resulting PEM files under `/etc/mielofon/` on the respective
controller (`node.pem`/`node.key` + `ca.pem`) and routers
(`agent.cer`/`agent.key` + `ca.pem`). Only placeholder names/addresses are
used above; no key material is ever committed here.

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
