# rpcd-mod-bird change notes (as implemented)

Background: this is the rpcd-side half of the mielofon agent link
auto-discovery design; see `docs/link-autodiscovery.md` for the
consumer-side rationale. Both changes below are **implemented** in the feed;
this file records the contract they established (so the mielofon side can
pin against the real field/method names). Sanitized examples throughout
(`mesh_v3`, `awg_hub_a`, `peer_hub_a_example_com`).

## Scope (both done)

1. `bird status` reports each OSPF interface's **type** (`ptp`/`broadcast`/…).
2. A dedicated **cost method** `bird set_ospf_cost` changes an OSPF
   interface cost without the caller building a raw BIRD CLI string.

`bird query` and `bird status` remain **backwards compatible**; the parser
helpers moved to `src/birdconfig.uc` (pure, unit-tested with the plugin).

## Change 1 — OSPF interface `type`

`parseOspfInterfaces()` (`src/birdconfig.uc`) emits `{interface, type, cost}`.

```
 Interface awg_hub_a (IID 0)
 	Type: ptp
 	Cost: 10
```
→

```json
{ "interface": "awg_hub_a", "type": "ptp", "cost": 10 }
```

Every field is optional: an interface without a `Type:` line yields
`type == null` and is **not** an error. Values pass through as-is
(`ptp`, `broadcast`, `nbma`, …), no normalization.

## Change 2 — `bird set_ospf_cost` (implemented contract)

Args (mirror `query`'s object style; `socket` optional):

```ucode
set_ospf_cost: { interface: 'awg_hub_a', cost: 25, socket: '/run/bird.ctl' }
```

Return: `{ code, stdout }`, success is `{ code: 0, stdout: "<bird output>" }`
and:
- `code 2` — bad args (`interface` empty, `cost` not a 1..65535 integer);
- `code 3` — the OSPF config editor could not bind the interface;
- `code 4` — file I/O error (config unreadable/unwritable);
- `code 5` — another reconfiguration in progress (flock busy);
- `code 1` — BIRD `configure check`/apply rejected the config.

Behaviour (OpenWrt-specific, kept for whoever maintains the feed later):

1. The **pristine** config is `/etc/bird.conf` and is never written.
2. A runtime config `/tmp/etc/bird.conf` carries cost overrides applied to
   other links; it is seeded from `/etc/bird.conf` when absent.
3. The config editor rewrites the OSPF interface cost in place
   (`editOspfCost`, `src/birdconfig.uc`).
4. The derived config is validated with `configure check` before being
   applied with `configure "/tmp/etc/bird.conf"`.
5. Reconfigurations are serialized with flock on `/run/bird.reconfigure.lock`
   (BIRD has a single config and one undo level).
6. BIRD 3.x forgets the last `configure` filename on a bare `birdc
   configure`/SIGHUP (re-reads `/etc/bird.conf`), so to reset applied costs
   the operator deletes `/tmp/etc/bird.conf`.
7. **No `protocol` argument**: the method binds the interface by name within
   the config; the consumer passes `interface` + `cost` only.

The method is idempotent: setting the same cost twice is a plain success.

## Consumer notes (agent side)

- The mielofon agent calls `bird set_ospf_cost {interface, cost}`; on
  devices still running an older rpcd-mod-bird without this method it falls
  back to `bird query` with an operator `cost_command` template. No raw
  BIRD CLI construction otherwise.
- `status.ospf[].interfaces[].type` is used to select `ptp` interfaces;
  `type == null` (older snapshot) degrades to the BGP-peer-name match.
- Do not send `protocol` to `set_ospf_cost`.

## Versioning

Feature release: `PKG_VERSION` `0.3.0 → 0.4.0`, `TITLE`/`description` now
mention `set_ospf_cost` and the interface `type` field. Field/method names
above are stable for the consumer.

## Verification

- Parser: unit-tested in `src/test` of the feed (also covered by the
  mielofon agent's `05_autodiscover` fixture for `type` tolerance).
- Live: `ubus call bird set_ospf_cost '{"interface":"<iface>","cost":X}'`
  then `birdc show ospf interface mesh_v3` must show the new cost; re-apply
  the old cost to restore.
- Live status: `ubus call bird status` per-node must show `awg_*` as
  `type == "ptp"` (mesh OSPF) and any `dummy_*` as `type == "broadcast"`;
  unrelated OSPF instances appear in a separate `status.ospf[]` entry.

## Deployment status

**Not yet deployed** on the target routers (live devices still expose only
`query` + `status`). The agent tolerates this via the `cost_command`
fallback; roll out the 0.4.0 plugin, then the agent can drop the fallback.