#!/usr/bin/env bash
set -euo pipefail

: "${TARGET_URL:?TARGET_URL is required}"
: "${TOKEN_0:?TOKEN_0 is required}"
: "${TOKEN_1:?TOKEN_1 is required}"
: "${HOT_ACCOUNT:?HOT_ACCOUNT is required}"

mkdir -p artifacts
k6 run release/load/mainnet-readiness.js

if [[ ! -f artifacts/capacity-report.json ]]; then
  echo "artifacts/capacity-report.json must be supplied from the staged ramp analysis" >&2
  exit 1
fi

python3 release/scripts/evaluate.py \
  --policy release/config/promotion-policy.json \
  --results artifacts/k6-summary.json \
  --capacity artifacts/capacity-report.json
