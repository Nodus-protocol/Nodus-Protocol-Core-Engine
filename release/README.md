# Mainnet readiness gates

This directory contains the reproducible staging release gate for the core
engine. It deliberately targets an already-deployed release candidate: the
harness never holds a signing key and transaction preparation endpoints only
build unsigned payloads.

## Prerequisites

- `k6` 0.49 or newer
- Python 3.11 or newer
- Toxiproxy (only required for fault injection)
- a staging deployment backed by the release-candidate contracts and durable
  Redis/database instances

Copy `release/config/staging.env.example` to an environment file and replace
every placeholder. Keep that file outside version control.

The staging ingress/engine must expose current queue depth as
`X-Queue-Depth` on API responses. Absence of this telemetry blocks promotion;
the harness will not silently interpret a missing signal as an empty queue.

## Run the capacity gate

```bash
set -a
. ./.env.staging
set +a
./release/scripts/run-capacity.sh
python3 release/scripts/evaluate.py \
  --policy release/config/promotion-policy.json \
  --results artifacts/k6-summary.json \
  --capacity artifacts/capacity-report.json
```

The workload ramps beyond the expected production rate so the resulting
report can identify both the sustainable rate and the first saturation stage.
It covers burst and steady quotes, mixed pool reads, unsigned transaction
preparation, idempotent duplicate payment requests, status polling, and list
polling that represents event/reconciliation catch-up. Both token directions,
hot and distributed accounts, large valid amounts, and deliberately stale
deadlines are included.

## Run fault scenarios

Deploy the candidate with Redis and RPC traffic routed through Toxiproxy, then:

```bash
./release/scripts/setup-proxies.sh
FAULT_TARGET=redis ./release/scripts/run-fault.sh interrupt
FAULT_TARGET=database ./release/scripts/run-fault.sh interrupt
FAULT_TARGET=rpc ./release/scripts/run-fault.sh slow
FAULT_TARGET=rpc ./release/scripts/run-fault.sh packet-loss
FAULT_TARGET=rpc ./release/scripts/run-fault.sh divergent
./release/scripts/run-fault.sh rate-limit
./release/scripts/run-fault.sh accepted-timeout
ENGINE_CONTAINER=nodus-engine ./release/scripts/run-fault.sh terminate
```

Each fault run executes the load profile during the disruption and evaluates
the same safety assertions afterward. `rate-limit`, `divergent`, and
`accepted-timeout` require the staging provider/mock control hooks documented
by `RATE_LIMIT_HOOK`, `DIVERGENT_RPC_HOOK`, and `ACCEPTED_TIMEOUT_HOOK`. Hooks
must return non-zero if the requested behaviour was not enabled.

## Produce and sign the release manifest

```bash
./release/scripts/create-manifest.sh artifacts/release-manifest.json
cosign sign-blob --yes \
  --output-signature artifacts/release-manifest.sig \
  artifacts/release-manifest.json
cosign verify-blob --signature artifacts/release-manifest.sig \
  --certificate artifacts/release-manifest.pem \
  --certificate-identity "$SIGNER_IDENTITY" \
  --certificate-oidc-issuer "$SIGNER_ISSUER" \
  artifacts/release-manifest.json
```

All referenced values are immutable identifiers: the image must use a digest,
source and ABI use SHA-256/Git hashes, and contract/provider values describe
the exact tested deployment. CI stores evidence as an artifact; production
promotion consumes that same signed bundle.

The final promotion decision combines every gate and fails closed:

```bash
python3 release/scripts/promote.py \
  --policy release/config/promotion-policy.json \
  --evidence artifacts/promotion-evidence.json
```

Use `release/examples/promotion-evidence.json` as the evidence shape. A release
manager may mark `signed_manifest_verified` only after the `cosign verify-blob`
command above succeeds. Every named fault must have its own archived k6 summary.

## Canary and rollback

The initial canary is limited by all of the following controls:

- at most 1% of eligible traffic and 60 requests per minute;
- at most 100 requests, 100 XLM total funding, and 0.1 XLM total fees;
- automatic abort on any duplicate submission or lost terminal result;
- automatic abort when error rate, queue depth, latency, recovery time, funding,
  or fee thresholds in `promotion-policy.json` are exceeded.

`run-canary.sh` polls the deployment's promotion-evidence endpoint throughout
the observation window and invokes the platform-specific `CANARY_ABORT_HOOK`
as soon as a fetch or threshold check fails:

```bash
CANARY_EVIDENCE_URL=https://staging.example.invalid/release-evidence \
CANARY_ABORT_HOOK='./ops/set-canary-weight 0' \
./release/scripts/run-canary.sh
```

On abort, set the canary route weight to zero, disable its signer/funding
policy, restore the last signed image/manifest, and keep reconciliation running
until every accepted transaction reaches one terminal state. Do not retry an
accepted request with a new idempotency key. The release manager records the
rollback time and reconciliation result before another attempt.
