# AGENTS.md

Engineering notes for this repo: the mielofon source **and** an OpenWrt feed.
The authoritative build spec is `mielofon-handoff.md`; read it before writing
code (its agent section is superseded: the agent is Rust now). Stage 1
(controller) and Stage 2 (agent) are implemented.

## Source of truth
- `mielofon-handoff.md` is the authoritative spec. `README.md` is a stub summary.
- User instructions in chat override both unless they conflict with safety constraints.

## CRITICAL: sanitization mandate
This is a **PUBLIC** repository describing software for a **private** production mesh. Never expose, in any file, commit, comment, doc, CI, test, or example:
- Real IPs (IPv4/IPv6), hostnames, hub/spoke/site names, ASNs, domains, OOB/ULA/mesh addressing.
- Use only placeholders (`hub-a`..`hub-e`, `node-0..4`) and RFC-private doc ranges (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`, `fd00::/8`, ASN `64512..65534`).
- Before committing, grep for literal `[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+` and real identifiers, and replace with placeholders.
- Never commit any private key, cert, or CA material — ship only the generation scheme/script with placeholders.

## Architecture (controller = decision-maker)
- The **controller is the responsible party** for the whole mesh: scheduler,
  probe fence, quality classification, cost policy, and work dispatch to agents.
- The **agent is a thin executor** (`crates/agent`, Rust): it dials a
  controller and keeps one **mTLS QUIC session**, registers its links on it,
  answers every command stream the controller opens (probes from its own
  continuous per-link ping window, or the gated throughput test) with a reply
  on the same stream **echoing the job id**, and applies the OSPF cost it is
  told to apply. It has no scheduling/policy logic and never acquires the
  fence. Spokes sit behind NAT → the agent dials, the controller never
  connects out; once the session is up the controller opens streams on it.
- Wire types live in `crates/mielofon-proto`, shared by controller and agent:
  change the contract there, never in one side only. The carrier (frames,
  sessions, QUIC) is `crates/mielofon-transport`; code above it only opens
  and accepts streams, so a TCP carrier can be added later.
- Wire flow: agent stream `register` → controller streams `probe{always|throughput}`
  / `apply_cost` / `trace` (each with an `id`), reply on the same stream
  (`kind: probe` echoes the fence token for throughput; `kind: applied` acks
  a cost). Control rides the underlay, never a mesh tunnel.

## Stack & conventions
- **Controller** is Rust (tokio + axum + rustls + quinn). Hubs run **glibc**; build/test
  natively. CI additionally builds a static `x86_64-unknown-linux-musl` artifact.
- **Agent** is Rust (tokio), built per target by the feed (`rust-package.mk`);
  agent nodes are x86_64, aarch64 and armv7 (musl). It pings in-process on raw
  ICMPv6 sockets (kernel RX timestamps) and runs two kinds of subprocess on
  purpose: `iperf3 -J` for the gated throughput test and the `ubus` CLI for
  UCI/netifd/rpcd-mod-bird calls (low-rate; no libubus binding). Never block
  the runtime: anything slow is a child process or its own task.
  BIRD is driven through
  [rpcd-mod-bird](https://github.com/vooon/my-openwrt-feed/tree/master/rpcd-mod-bird)
  — its source is the contract, not `docs/rpcd-mod-bird-handoff.md`.
- **This repo is both the source AND an OpenWrt feed.** The top-level
  `mielofon-controller/` and `mielofon-agent/` dirs are `Package/` feed
  definitions. OpenWrt's feed scanner walks the whole repo for Makefiles defining
  `Package/`, so `crates/`/`docs/` are ignored by the feed. Mirror conventions
  from `vooon/my-openwrt-feed`.
- Source layout: `crates/controller/`, `crates/agent/`, `crates/mielofon-proto/`,
  `crates/mielofon-otel/`, `docs/`, `.github/workflows/`.
- Runtime toolchain (OpenWrt, OSPF/BIRD mesh config) and the Ansible integration
  live in a separate private repo — out of scope here.

## Commands
- Build controller: `cargo build --package mielofon-controller` (native)
- Static CI artifact: `cargo build --release --package mielofon-controller --target x86_64-unknown-linux-musl`
- Verify (Rust): `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- Agent raw-socket tests (need CAP_NET_RAW; ignored by default): build with
  `cargo test -p mielofon-agent --no-run`, then
  `unshare -rn sh -c 'ip link set lo up && <test-binary> --ignored icmp::'`
- `shellcheck mielofon-agent/files/etc/init.d/mielofon-agent`
- Feed Makefiles: `mielofon-controller/Makefile` and `mielofon-agent/Makefile`
  (`rust/host` + `include ../../packages/lang/rust/rust-package.mk`).

## Critical rules
- User directives are absolute: if the user says `DO NOT <action>`, do not perform that action without explicit permission.
- Never edit credential/config files or fabricate/overwrite credentials unless the user explicitly asks.
- Preserve user data and configuration; ask before changing if in doubt. Do not undo user choices in favor of your own approach without discussing first.
- Keep links absolute unless requested otherwise; prefer minimal, targeted patches.
- Every text file must end with a final newline (`\n`) — no trailing-newline-less files.

## Commit messages
- Conventional Commits: `<type>(<scope>): <description>` (see https://www.conventionalcommits.org/en/v1.0.0/#summary).
- The Ansible-specific rules in earlier revisions (clouds.yml, inventory/group vars, role defaults) do not apply to this repo.
