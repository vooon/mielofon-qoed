# rpcd-mod-bird change spec (handoff for the plugin maintainer)

This document is written for the developer/agent who will modify
`rpcd-mod-bird`. It is self-contained: no prior mielofon knowledge is
assumed beyond the plugin itself. It is the rpcd-side half of the
mielofon agent link auto-discovery design; see `docs/link-autodiscovery.md`
for the consumer-side rationale.

## Scope

Change the ucode rpcd plugin `rpcd-mod-bird` (file `src/bird.uc` in the
feed, installed to `/usr/share/rpcd/ucode/bird.uc`) twice:

1. Extend the OSPF interface parser so `bird status` reports each OSPF
   interface's **type** (`ptp` / `broadcast` / …).
2. Add a dedicated **cost-set method** so callers can change an OSPF
   interface cost without building a raw BIRD CLI command string.

Keep the existing `bird query` and `bird status` methods **backwards
compatible** — other consumers use them.

## Conventions

- The plugin registers one ubus object, `bird`, with methods `query` and
  `status`, exposed like:

  ```ucode
  const methods = { query: {...}, status: {...} };
  return { bird: methods };
  ```

- BIRD socket I/O and reply handling are already implemented
  (`birdRaw()`, `replyComplete()`, `cleanReply()`) — reuse them. Do not add
  another raw-socket path.
- Parsing uses ucode `match()`/POSIX regex, consistent with
  `prometheus-node-exporter-ucode`'s `bird.uc` (must not differ between
  musl/glibc).

## Change 1 — report OSPF interface `type`

### Where

`parseOspfInterfaces()` in `src/bird.uc`.

### Current shape (do not break)

Per OSPF interface, `show ospf interface <proto>` prints blocks like
(sanitized):

```
 Interface awg_hub_a (IID 0)
 	Type: ptp
 	Area: 0.0.0.0 (0)
 	State: PtP
 	Cost: 10
```

The parser currently emits only:

```json
{ "interface": "awg_hub_a", "cost": 10 }
```

### New shape

Emit the `Type:` line as well, when present:

```json
{ "interface": "awg_hub_a", "type": "ptp", "cost": 10 }
```

- Add a case like `if ((m = match(l, /^Type:[ \t]+(\S+)/))) ifa.type = m[1];`
  alongside the existing `^Interface` / `^Cost` cases (the loop already
  strips leading whitespace per line).
- All three fields optional on parse: if `Type:` is missing for some output
  variant, `type` is simply `null` — never fail the whole `status` call.
- `broadcast`, `ptp`, `nbma` etc. come straight through as-is; **no
  normalization**.

## Change 2 — new `bird set_cost` method

### Purpose

Currently the consumer (mielofon agent) applies cost by sending a raw BIRD
command through `bird query` (a `configure`-style string with
`{interface}`/`{cost}` placeholders it builds itself). That leaks feed
internals into the agent and forces per-link command templates in consumer
config. Replace that path with a structured method.

### Contract

```ucode
set_cost: {
    args: {
        protocol: '',      // OSPF protocol name, e.g. "mesh_v3" (see below)
        interface: '',     // e.g. "awg_hub_a"
        cost: 0,           // integer OSPF cost
        socket: '',        // optional, defaults to /run/bird.ctl
    },
    call: function(request) { ... },
}
```

Response (mirror `query`'s shape): success returns
`{ code: 0, stdout: "<bird output>" }`; any failure (bad args, socket
error, non-zero BIRD reply) returns `{ code: <non-zero>, stdout: … }` or
`{ code: 1 }` when the socket is unreachable.

### Behaviour

1. Validate args: `interface` and `cost` are required; `cost` must be a
   non-negative integer. Invalid args → `{ code: 2 }`.
2. Unless `protocol` is given, operate on the single OSPF instance; if more
   than one OSPF protocol exists and `protocol` is omitted, return
   `{ code: 3, stdout: "multiple OSPF protocols; specify protocol" }`.
3. Build and run the BIRD command that sets the OSPF interface cost via the
   existing `birdRaw()` path. **Use the same reconfiguration mechanism the
   feed already uses for cost changes** (the same `configure`-style command
   that today's `cost_command` template produces) — do not invent a new
   one. Substitute the interface name and the cost value into that command.
4. Return the cleaned BIRD reply as `stdout` with `code` set from BIRD's
   reply code (0 on success).

### Notes / decisions to settle

- Method name here is `set_cost`; if the feed prefers `set_ospf_cost`,
  rename consistently (consumer follows the real name — it is parameterized
  in `docs/link-autodiscovery.md`).
- Whether `protocol:` is mandatory everywhere or only when ambiguous: pick
  one and document it; the consumer will always pass it, so either works.
- Keep `query` untouched — the agent no longer uses it for cost, but it has
  other consumers.
- `set_cost` must be idempotent: applying the same cost twice is a no-op
  success.

## Versioning

This is a feature release of the plugin. Bump `PKG_VERSION` (e.g.
`0.3.0 → 0.4.0`) in the feed `Makefile`, and update the package `TITLE`/
`description` to mention `set_cost` and the interface `type` field. The
consumer pins against these field names; keep `status`/`query` field names
stable.

## Verification

- **Unit (parser):** extend the plugin's test approach (if any) or run the
  new interface, feeding it the `show ospf interface` block above via a
  fake socket; assert `{interface, type, cost}` come back correctly and that
  an interface without `Type:` yields `type == null`.
- **Live (socket):** on a router, `birdc show ospf interface mesh_v3` must
  match what `ubus call bird status` reports (types present), and each node
  exposes the known interfaces:
  - mesh OSPF: interfaces named `awg_*`, `type == "ptp"`, plus a
    `dummy_*`/`type == "broadcast"` stub;
  - nodes running a second, unrelated OSPF: those interfaces must appear in
    a different `status.ospf[]` entry (per `protocol`), so consumers can
    filter by protocol name.
- **Live (set_cost):** call
  `ubus call bird set_cost '{"protocol":"mesh_v3","interface":"<iface>","cost":X}'`
  then `birdc show ospf interface mesh_v3` and confirm the interface's cost
  changed; re-apply the old cost to restore.

## Sanitization

This feed is private but memorable bits still matter for the consumer's
public repo. Keep examples generic (`mesh_v3`, `awg_hub_a`,
`peer_hub_a_example_com`); no real hostnames, ASNs, or IPs in code
comments, logs, or README. Pipe real data through examples only, never into
the payload ucode.