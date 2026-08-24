#!/usr/bin/env bash
set -euo pipefail

: "${REDIS_UPSTREAM:?REDIS_UPSTREAM is required}"
: "${DATABASE_UPSTREAM:?DATABASE_UPSTREAM is required}"
: "${RPC_UPSTREAM:?RPC_UPSTREAM is required}"
url="${TOXIPROXY_URL:-http://127.0.0.1:8474}"

create() {
  curl --fail --silent --show-error -X POST -H 'Content-Type: application/json' \
    -d "{\"name\":\"$1\",\"listen\":\"0.0.0.0:$2\",\"upstream\":\"$3\"}" "$url/proxies"
}

create redis 6379 "$REDIS_UPSTREAM"
create database 5432 "$DATABASE_UPSTREAM"
create rpc 8000 "$RPC_UPSTREAM"
