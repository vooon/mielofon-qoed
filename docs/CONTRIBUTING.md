# Contributing

## Sanitization is mandatory

This is a **public** repository describing software for a **private** mesh.
Never expose, in any file, commit, comment, doc, CI, test, or example:

- Real IP addresses (IPv4/IPv6), hostnames, hub/spoke/site names, ASNs,
  domains, OOB/ULA/mesh addressing.
- Use only placeholders (`hub-a`..`hub-e`) and RFC-private documentation
  ranges (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`,
  `fd00::/8`, private ASN `64512..65534`).
- Never commit any private key, cert, or CA material — ship only the generation
  scheme with placeholders.

Before pushing, run the sanitization scans (also enforced by CI):

```sh
git grep -nE '\b([0-9]{1,3}\.){3}[0-9]{1,3}\b'
```

and replace any hits with placeholders.

## Build & verify

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Run the controller locally (hubs are glibc; build the native target):

```sh
cargo build --package mielofon-controller
./target/debug/mielofon-controller /path/to/config.toml
```

A static musl artifact is produced by CI:

```sh
cargo build --release --package mielofon-controller --target x86_64-unknown-linux-musl
```

## Feed

The repo is itself an OpenWrt feed. Makefiles live in `mielofon-controller/`
(Rust, built in-feed) and `mielofon-agent/`. Keep `Cargo.lock` in sync.

## Commits

Conventional Commits: `<type>(<scope>): <description>`. Scope examples:
`controller`, `otel`, `agent`, `feed`, `docs`.

## Notes

- `rustls` is used with the `ring` provider; a process-level default is
  installed at startup. Keep the ring provider consistent across
  workspace-level TLS dependencies.
- OpenTelemetry is HTTP transport only. Do not enable the `grpc-tonic` feature.
