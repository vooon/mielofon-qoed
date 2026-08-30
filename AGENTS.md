# AGENTS.md

Engineering notes for this repo: the mielofon source **and** an OpenWrt feed.
The authoritative build spec is `mielofon-handoff.md`; read it before writing
code. Stage 1 (controller) and Stage 2 (ucode agent) are implemented.

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
- The **agent is a thin executor** (`mielofon-agent/`, ucode): it registers, then
  **long-polls** `POST /v1/agent/command` for commands, runs the probe it is told
  to run (or applies the OSPF cost it is told to apply), and replies on
  `POST /v1/agent/reply`, **echoing the job id** of every command. It has no
  scheduling/policy logic and never acquires the fence. Spokes sit behind NAT →
  agent-pull (long-poll) only; the controller never connects out.
- Wire flow: `register` → long-poll `command` (`probe{always|throughput}` +
  `apply_cost`, each with an `id`) → `reply` (`kind: probe` echoes the fence
  token for throughput; `kind: applied` acks a cost).

## Stack & conventions
- **Controller** is Rust (tokio + axum + rustls). Hubs run **glibc**; build/test
  natively. CI additionally builds a static `x86_64-unknown-linux-musl` artifact.
- **Agent** is **ucode** (OpenWrt scripting) — no Python, no Rust. Feed
  `mielofon-agent` is `PKGARCH:=all` and installs `.uc` modules only. Probes run
  the resident `ping`/`iperf3`/`netperf` binaries via `fs.popen()`
  (`uloop.process` stdout capture is unreliable in the target snapshot); BIRD is
  driven over ubus through `rpcd-mod-bird` (no `ucode-mod-socket` needed by the
  agent). mTLS to the controller uses `ucode-mod-uclient` **built with SSL** —
  the package's KConfig guarantees a `libustream-*` backend is always enabled
  (openssl preferred, mbedtls fallback). The uclient transport constructor is
  injected into `client.uc` (`mielofon-agent/files/.../transport.uc`) so the
  reserved `new` name stays contained and requests are unit-testable.
- **This repo is both the source AND an OpenWrt feed.** The top-level
  `mielofon-controller/` and `mielofon-agent/` dirs are `Package/` feed
  definitions. OpenWrt's feed scanner walks the whole repo for Makefiles defining
  `Package/`, so `crates/`/`docs/` are ignored by the feed. Mirror conventions
  from `vooon/my-openwrt-feed`.
- Source layout: `crates/controller/`, `crates/mielofon-otel/`, `mielofon-agent/`
  (ucode package), `docs/`, `.github/workflows/`.
- Runtime toolchain (OpenWrt, OSPF/BIRD mesh config) and the Ansible integration
  live in a separate private repo — out of scope here.

## ucode (the agent is written in ucode; NOT JavaScript)
The agent (`mielofon-agent/`) is ucode. These rules were hard-learned in
`obserwrt` — do not reinvent them. ucode is ECMAScript-inspired but a distinct
language with a smaller stdlib; the official docs are authoritative:
https://ucode.mein.io (Usage, Syntax, module-{core,log,uci,ubus,uloop,uclient}).

- `"use strict";` at the top of every module.
- `export function f(){…}` must end with `;`; import relative modules with
  `import { f } from './f.uc'` and native modules with `import { x } from 'uci'`
  or `import * as m from 'socket'`.
- No function hoisting — declare before use. Do **not** use `function f;`
  forward-declarations (the export form is unsupported and plain ones shadow).
- No `throw` (use `die()`), no `const` in loop heads, no adjacent-string concat.
- Arrays use global functions: `push(arr,…)`, `filter`, `map`, `pop` — no
  `arr.push()` methods.
- Strings are not `[]`-indexable — use `substr(s,i,1)` / `ord(s,i)`.
- `for (x in arr)` yields elements; over objects it yields keys. Guard before
  iterating objects that may be null.
- `uci` option values are strings — `int(...)`/explicit truthy (`v == '1'`).
- JSON: decode with `json(str)`, encode with `sprintf("%J", obj)`.
- Loop variables must be declared: `for (let x in arr)` (bare `for (x in …)` is a
  runtime error; `for..in` yields elements).
- Object/method bodies closing over a variable used in their own initializer are
  rejected at parse ("use before initialization") — declare `let x = null;`
  first, assign later.
- Run probes via `fs.popen()` + pipe reads (the deployment snapshot's
  `uloop.process` stream API is unreliable); keep the event loop responsive
  (single-threaded).

## Commands
- Build controller: `cargo build --package mielofon-controller` (native)
- Static CI artifact: `cargo build --release --package mielofon-controller --target x86_64-unknown-linux-musl`
- Verify (Rust): `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`
- Agent lint: `node scripts/uc-lint.mjs` (ucode ESM parse + ucode rules); unit
  tests: `mielofon-agent/tests/run_tests.sh` (ucode + mocked modules);
  `shellcheck mielofon-agent/files/etc/init.d/mielofon-agent`
- Feed Makefiles: `mielofon-controller/Makefile`
  (`rust/host` + `include ../../packages/lang/rust/rust-package.mk`),
  `mielofon-agent/Makefile` (pure ucode, `PKGARCH:=all`, no build).

## Critical rules
- User directives are absolute: if the user says `DO NOT <action>`, do not perform that action without explicit permission.
- Never edit credential/config files or fabricate/overwrite credentials unless the user explicitly asks.
- Preserve user data and configuration; ask before changing if in doubt. Do not undo user choices in favor of your own approach without discussing first.
- Keep links absolute unless requested otherwise; prefer minimal, targeted patches.

## Commit messages
- Conventional Commits: `<type>(<scope>): <description>` (see https://www.conventionalcommits.org/en/v1.0.0/#summary).
- The Ansible-specific rules in earlier revisions (clouds.yml, inventory/group vars, role defaults) do not apply to this repo.
