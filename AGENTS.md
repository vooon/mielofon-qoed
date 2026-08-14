# AGENTS.md

Engineering notes for this repo. The repo is currently **empty** (no Cargo project or code yet) and is being bootstrapped from the authoritative build spec in `mielofon-handoff.md`. Read that first before writing any code.

## Source of truth
- `mielofon-handoff.md` is the authoritative spec for what to build (components, APIs, layout, acceptance criteria). `README.md` is only a one-line stub.
- User instructions in chat override both unless they conflict with safety constraints.

## CRITICAL: sanitization mandate
This is a **PUBLIC** repository describing software for a **private** production mesh. Never expose, in any file, commit, comment, doc, CI, test, or example:
- Real IPs (IPv4/IPv6), hostnames, hub/spoke/site names, ASNs, domains, OOB/ULA/mesh addressing.
- Use only placeholders (`hub-a`..`hub-e`, `node-0..4`) and RFC-private doc ranges (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`, `fd00::/8`, ASN `64512..65534`).
- Before committing, grep for literal `[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+` and real identifiers, and replace with placeholders.
- Never commit any private key, cert, or CA material — ship only the generation scheme/script with placeholders.

## Stack & conventions
- **Controller** is Rust, built as a single **static** binary for `x86_64-unknown-linux-musl` (verify with `ldd`: no dynamic deps).
- **Agent** is pure **ucode** (OpenWrt scripting) — no Python. ucode modules: `uloop`/`utoop`, `uci`, `socket`, `fs`, `math`, `log`.
- Planned layout (from handoff): `crates/controller/`, `agent/ucode/`, `docs/`, `.github/workflows/`.
- Runtime toolchain (OpenWrt, OSPF/BIRD mesh config) and the Ansible integration live in a separate private repo — out of scope here.

## Commands (as specified in handoff; repo not yet scaffolded)
- Build controller: `cargo build --release --target x86_64-unknown-linux-musl`
- Verify: `cargo fmt`, `cargo clippy`, `cargo test`
- Provide both OpenWrt `Makefile`s when scaffolding packaging.

## Critical rules
- User directives are absolute: if the user says `DO NOT <action>`, do not perform that action without explicit permission.
- Never edit credential/config files or fabricate/overwrite credentials unless the user explicitly asks.
- Preserve user data and configuration; ask before changing if in doubt. Do not undo user choices in favor of your own approach without discussing first.
- Keep links absolute unless requested otherwise; prefer minimal, targeted patches.

## Commit messages
- Conventional Commits: `<type>(<scope>): <description>` (see https://www.conventionalcommits.org/en/v1.0.0/#summary).
- The Ansible-specific rules in earlier revisions (clouds.yml, inventory/group vars, role defaults) do not apply to this repo.