# Protocol

Wiring between the controller, agents, and cluster members. All bodies are
JSON. Endpoints on the **members** (`9551`) and **clients** (`9552`) listeners
are mTLS; the read endpoints on the **admin** listener (`9553`) are loopback
HTTP.

## Division of responsibilities

The **controller is the decision-maker** for the whole mesh: it schedules probe
work, holds the probe fence, classifies quality, derives the OSPF cost policy,
and dispatches commands to agents.

The **agent is a thin executor**: it runs whatever probe it is told to run and
reports raw measurements, and it applies the OSPF cost it is told to apply.
It holds **no** scheduling, policy, or classification logic and never acquires
the fence itself.

Because spokes sit behind NAT, the controller→agent channel is **agent-pull**:
an agent registers, polls the working queue, executes commands, reports, and
acks. The controller never connects out to an agent.

```
┌──────────── controller ────────────┐
 scheduler → fence → dispatch work    │   POST /v1/agent/work  (pull)
 classifier → policy → apply_cost     │◄────────────────────────── agent
 quality store (KV) ← reports          │   POST /v1/quality
└─────────────────────────────────────┘
```

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

`state` is `quiet` | `busy` | `conflict`. `quality`
(`good|acceptable|poor|bad`) and `ospf_cost` are assigned by the controller.

## Agent lifecycle

### Register

```jsonc
POST /v1/agent/register           // clients
{ "agent": "spoke-1",
  "links": [ {"from":"spoke-1","to":"hub-a","interface":"awg_hub_a"} ] }
// -> { "ok": true, "commands": [ {apply_cost …} ] }   // current policy snapshot
```

### Poll work

```jsonc
POST /v1/agent/work               // clients
{ "agent": "spoke-1" }
// -> { "commands": [ … ] }        // drained; empty when nothing is due
```

Command shapes:

```jsonc
{ "id": "f8a2…", "type": "probe", "tier": "always",
  "link": { "from":"spoke-1","to":"hub-a","interface":"awg_hub_a" } }

{ "id": "9c11…", "type": "probe", "tier": "throughput",
  "token": "6e4a…",                    // fence lease held by the controller
  "link": { … } }

{ "id": "3d77…", "type": "apply_cost",
  "link": { … }, "cost": 50 }
```

The controller's scheduler issues `probe/always` per link on an interval, and
`probe/throughput` only when the link's fence lease is free — it acquires the
lease before issuing the command. `apply_cost` is issued when the derived cost
for a link differs from the last cost the controller told the agent to apply.

### Report

```jsonc
POST /v1/quality                 // clients
{ "link": {…},
  "rtt_ms":15.0, "loss_pct":0.0, "rr_tps":90.0,
  "tcp_mbps":1.5, "udp_mbps":null, "util_mbps":0.0,
  "state":"quiet", "token": null }
// -> { "accepted":true, "quality":"poor", "ospf_cost":50 }
```

- Always-on reports carry no token. Throughput reports echo the fence token
  from the command; the controller releases the lease on receipt.
- A busy link is reported `state: "busy"` (with `util_mbps`) and is never
  classified as degraded.

### Apply ack

```jsonc
POST /v1/apply/ack               // clients
{ "agent":"spoke-1", "link":{…}, "cost":50 }
// -> { "ok": true }
```

## Fence (soft lease)

Owned entirely by the controller scheduler. A lease has a TTL and token; on
expiry without a report it may be re-issued. An overlapping throughput probe is
tolerated and reported as `conflict`.

## Read endpoints (admin listener)

```text
GET /v1/quality?from=..&to=..&interface=..   latest record for a link
GET /v1/quality/all                          full replicated view
GET /v1/policy?from=..&to=..&interface=..    quality + ospf_cost decision
GET /v1/status                               node, readiness, leases, agents
GET /healthz                                 200 when process is up
GET /readyz                                  200 when ready, 503 otherwise
GET /metrics                                 Prometheus text exposition
```

## Gossip (anti-entropy)

Members exchange the full KV view. LWW merge keeps the newest `ts` per link;
records older than `grace_ttl_secs` expire.

```jsonc
POST /v1/gossip/exchange          // members
{ "node": "hub-a", "records": [[…]] }
// -> { "node": "hub-b", "records": […] }
```