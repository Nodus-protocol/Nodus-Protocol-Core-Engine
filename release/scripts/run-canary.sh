#!/usr/bin/env bash
set -euo pipefail

: "${CANARY_EVIDENCE_URL:?CANARY_EVIDENCE_URL is required}"
: "${CANARY_ABORT_HOOK:?CANARY_ABORT_HOOK is required}"
interval="${CANARY_POLL_SECONDS:-10}"
duration="${CANARY_DURATION_SECONDS:-600}"
deadline=$((SECONDS + duration))
mkdir -p artifacts

while (( SECONDS < deadline )); do
  if ! curl --fail --silent --show-error "$CANARY_EVIDENCE_URL" \
    -o artifacts/promotion-evidence.json \
    || ! python3 release/scripts/promote.py \
      --policy release/config/promotion-policy.json \
      --evidence artifacts/promotion-evidence.json; then
    echo "canary threshold failed; executing abort hook" >&2
    bash -lc "$CANARY_ABORT_HOOK"
    exit 1
  fi
  sleep "$interval"
done

echo "canary observation window passed"
