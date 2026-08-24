#!/usr/bin/env bash
set -euo pipefail

scenario="${1:?usage: run-fault.sh SCENARIO}"
toxiproxy_url="${TOXIPROXY_URL:-http://127.0.0.1:8474}"
target="${FAULT_TARGET:-rpc}"

add_toxic() {
  curl --fail --silent --show-error -X POST \
    -H 'Content-Type: application/json' \
    -d "$2" "$toxiproxy_url/proxies/$target/toxics"
}

cleanup() {
  curl --silent -X DELETE "$toxiproxy_url/proxies/$target/toxics/readiness-fault" >/dev/null || true
}
cleanup_all() {
  cleanup
  if [[ -n "${k6_pid:-}" ]]; then
    kill "$k6_pid" 2>/dev/null || true
  fi
}
trap cleanup_all EXIT

apply_hook=""
mkdir -p artifacts
k6 run release/load/mainnet-readiness.js &
k6_pid=$!
sleep "${FAULT_WARMUP_SECONDS:-10}"

case "$scenario" in
  interrupt)
    add_toxic "$target" '{"name":"readiness-fault","type":"timeout","stream":"downstream","attributes":{"timeout":10000}}'
    ;;
  slow)
    add_toxic "$target" '{"name":"readiness-fault","type":"latency","stream":"downstream","attributes":{"latency":2000,"jitter":500}}'
    ;;
  packet-loss)
    add_toxic "$target" '{"name":"readiness-fault","type":"limit_data","stream":"downstream","toxicity":0.20,"attributes":{"bytes":1}}'
    ;;
  divergent)
    : "${DIVERGENT_RPC_HOOK:?DIVERGENT_RPC_HOOK is required}"
    apply_hook="$DIVERGENT_RPC_HOOK"
    ;;
  rate-limit)
    : "${RATE_LIMIT_HOOK:?RATE_LIMIT_HOOK is required}"
    apply_hook="$RATE_LIMIT_HOOK"
    ;;
  accepted-timeout)
    : "${ACCEPTED_TIMEOUT_HOOK:?ACCEPTED_TIMEOUT_HOOK is required}"
    apply_hook="$ACCEPTED_TIMEOUT_HOOK"
    ;;
  terminate)
    : "${ENGINE_CONTAINER:?ENGINE_CONTAINER is required}"
    docker kill --signal KILL "$ENGINE_CONTAINER"
    docker start "$ENGINE_CONTAINER"
    ;;
  *) echo "unknown fault scenario: $scenario" >&2; exit 2 ;;
esac

if [[ -n "$apply_hook" ]]; then
  bash -lc "$apply_hook"
fi

sleep "${FAULT_DURATION_SECONDS:-30}"
cleanup

recovery_started=$SECONDS
until curl --fail --silent --show-error "$TARGET_URL/healthz" >/dev/null; do
  if (( SECONDS - recovery_started >= 120 )); then
    echo "engine failed to recover within 120 seconds" >&2
    kill "$k6_pid" 2>/dev/null || true
    exit 1
  fi
  sleep 2
done
recovery_seconds=$((SECONDS - recovery_started))
wait "$k6_pid"
k6_pid=""

python3 - "$recovery_seconds" <<'PY'
import json, sys
from pathlib import Path

path = Path("artifacts/capacity-report.json")
report = json.loads(path.read_text())
report["recovery_seconds"] = int(sys.argv[1])
path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
PY

python3 release/scripts/evaluate.py \
  --policy release/config/promotion-policy.json \
  --results artifacts/k6-summary.json \
  --capacity artifacts/capacity-report.json
