# Financial-path operations

`/healthz` answers only whether the process can serve HTTP. `/readyz` removes
the instance from traffic when its network, signed manifest, contract spec,
provider freshness, Redis durability, contract configuration, or reconciliation
lag is unsafe. `/metrics` exposes low-cardinality operational signals; payload
data and identifiers are never labels.

## Alert response

| Alert | User impact | Stop condition | Diagnose | Safe recovery |
|---|---|---|---|---|
| RPC outage | submissions cannot be accepted or confirmed | pause preparation/submission | compare provider health and correlation-ID traces | fail over to a non-divergent provider, then reconcile before resuming |
| Redis loss | retries can duplicate work after restart | remove instance from readiness | verify Redis `PING` and connection saturation | restore Redis; never retry accepted work with a new key |
| Stale ledger | quotes and simulations may be unsafe | stop quotes and transaction preparation | compare source ledger and age across providers | wait for convergence and discard stale quotes |
| Reconciliation backlog | terminal results may be delayed or lost | stop new submissions at the SLO threshold | inspect job correlation IDs and queue depth | drain from the durable cursor; do not skip events |

Rollback to the last verified release/manifest when dependency recovery does
not restore readiness inside the SLO window. Never log or attach tokens,
signatures, XDR, amounts, balances, or full account addresses to an incident.

## Staging alert drill

For each drill, record UTC start/detection/page/mitigation/recovery times and
links to the alert and dashboard snapshot in the mainnet-readiness record.

1. Block RPC egress; confirm `RpcOutage`, failed submissions, and not-ready.
2. Stop Redis; confirm `RedisLoss`, no duplicate submission, and not-ready.
3. Proxy a stale ledger response; confirm `StaleLedger` and quote stop.
4. Pause reconciliation; confirm backlog alert and submission stop.
5. Restore each dependency, reconcile accepted work, and retain the timeline
   plus alert evidence with the signed release manifest.
