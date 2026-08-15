# Protocol

Wiring between the controller, agents, and cluster members. All bodies are
JSON. Endpoints on the **members** (`9551`) and **clients** (`9552`) listeners
are mTLS; the read endpoints on the **admin** listener (`9553`) are loopback
HTTP.

## Quality record

A directed link `{from, to, interface}` maps to a measurement record:

```json
{
  "ts": 1747000000,
  "rtt_ms": 15.0,
  "loss_pct": 0.0,
  "rr_tps": 90.0,
  "tcp_mbps": 1.5,
  "udp_mbps": null,
  "util_mbps": 0.0,
  "state": "quiet",
  "quality": "poor",
  "ospf_cost": 50
}
```

`state` is `quiet` | `busy` | `conflict` (reported by the agent). `quality`
(`good|acceptable|poor|bad`) and `ospf_cost` are assigned by the controller;
they are absent while unknown.

## Fence (soft lease)

Only the throughput tier is gated. The controller grants a lease with a TTL and
token. On expiry without release, another agent may take over. A second
acquirer receives `ok: false, reason: "held"` plus the current `holder`. An
overlap (eventual consistency) is tolerated and reported as `conflict`.

```jsonc
POST /v1/fence/acquire            // clients
{ "agent": "spoke-1", "link": "spoke-1-cr", "ttl_secs": 120 }
// -> { "ok": true,  "token": "...", "ttl": 120 }
// -> { "ok": false, "holder": "spoke-1", "reason": "held" }

POST /v1/fence/release            // clients
{ "link": "spoke-1-cr", "token": "..." }
// -> { "ok": true }
```

## Agent → controller

```jsonc
POST /v1/quality                  // clients
{ "link": {"from":"spoke-1","to":"cr","interface":"awg_cr"},
  "rtt_ms":15.0, "loss_pct":0.0, "rr_tps":90.0,
  "tcp_mbps":1.5, "udp_mbps":null, "util_mbps":0.0,
  "state":"quiet" }
// -> { "accepted":true, "quality":"poor", "ospf_cost":50 }
```

A busy link must be reported `state: "busy"` (with `util_mbps` set) and is not
classified — it is never reported as degraded.

## Read endpoints (admin listener)

```text
GET /v1/quality?from=..&to=..&interface=..   latest record for a link
GET /v1/quality/all                          full replicated view
GET /v1/policy?from=..&to=..&interface=..    quality + ospf_cost decision
GET /v1/status                               node, readiness, leases, members
GET /healthz                                 200 when process is up
GET /readyz                                  200 when ready, 503 otherwise
GET /metrics                                 Prometheus text exposition
```

## Gossip (anti-entropy)

Members exchange the full KV view. LWW merge keeps the newest `ts` per link;
the store expires records older than `grace_ttl_secs`.

```jsonc
POST /v1/gossip/exchange          // members
{ "node": "hub-a", "records": [[{"from":..,"to":..,"interface":..}, {...}]] }
// -> { "node": "hub-b", "records": [...] }
```

Each node pushes its view to every other member on a fixed interval
(`gossip_interval_secs`). Pushes tolerate a flapping fabric — a failed push is
simply retried next interval.