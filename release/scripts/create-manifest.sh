#!/usr/bin/env bash
set -euo pipefail

output="${1:-artifacts/release-manifest.json}"
: "${IMAGE:?IMAGE digest is required}"
: "${SOURCE_REVISION:?SOURCE_REVISION is required}"
: "${ABI_SHA256:?ABI_SHA256 is required}"
: "${CONTRACT_ID:?CONTRACT_ID is required}"
: "${PROVIDERS:?PROVIDERS is required}"
: "${TEST_RESULT_SHA256:?TEST_RESULT_SHA256 is required}"

case "$IMAGE" in *@sha256:*) ;; *) echo "IMAGE must be pinned by sha256 digest" >&2; exit 1 ;; esac
mkdir -p "$(dirname "$output")"
python3 - "$output" <<'PY'
import json, os, sys
from datetime import datetime, timezone

manifest = {
    "schema_version": 1,
    "created_at": datetime.now(timezone.utc).isoformat(),
    "image": os.environ["IMAGE"],
    "source_revision": os.environ["SOURCE_REVISION"],
    "abi_sha256": os.environ["ABI_SHA256"],
    "contracts": [os.environ["CONTRACT_ID"]],
    "providers": os.environ["PROVIDERS"].split(","),
    "test_result_sha256": os.environ["TEST_RESULT_SHA256"],
}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(manifest, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
