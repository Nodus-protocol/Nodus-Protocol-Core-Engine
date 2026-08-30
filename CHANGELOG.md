# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial public release
- Backend-to-Engine signed request authentication: HMAC-SHA256 signatures
  binding method, path, body digest, timestamp, nonce, scope, and network;
  durable (Redis-backed, with in-memory fallback) replay protection; and
  per-endpoint scopes (`read`, `tx_construct`, `tx_submit`, `admin`,
  `diagnostics`). All non-health endpoints now require a valid signature.
- Explicit CORS allow-list (`CORS_ALLOWED_ORIGINS`) — no origins configured
  means no CORS layer is attached at all, replacing the previous permissive
  `Any`/`Any`/`Any` policy.
- Per-workload, per-scope rate limiting and per-scope concurrency limits.
- Global request body size limit.
- Startup guard requiring `TLS_TERMINATED_UPSTREAM=true` and at least one
  configured service key before running with `NETWORK=mainnet`.
- Soroban transaction preparation for the pool contract's `swap`,
  `add_liquidity`, and `remove_liquidity` calls: typed XDR construction via
  the official `stellar-xdr`/`stellar-strkey` crates against an embedded,
  hashed ABI manifest (`src/pool/abi.rs`), Soroban RPC simulation (resource
  footprint, transaction data, authorization entries, minimum resource
  fee), and a machine-readable review summary decoded straight back out of
  the prepared XDR.
- `POST /api/v1/pool/validate` — decodes and policy-checks any prepared
  transaction XDR (known ABI, single operation, correct contract/network,
  non-expired deadline, fee within `POOL_FEE_CEILING_STROOPS`, non-stale
  sequence).
- `POST /api/v1/pool/submit` — engine-owned submission: relays a
  caller-signed transaction to Soroban RPC `sendTransaction` and polls
  `getTransaction`. The engine never signs; submission ownership is
  explicit per endpoint.
- `POOL_BASE_FEE_STROOPS` / `POOL_FEE_CEILING_STROOPS` configuration.
- Golden tests (`tests/soroban_prepare_test.rs`) covering swap/add/remove
  happy paths and adversarial mutations: unknown ABI function, extra
  operations, wrong contract, expired deadline, fee above ceiling, and
  stale sequence — run against a faked Soroban RPC transport, no network
  required.

### Changed
- `/api/v1/pool/build/swap`, `/build/add-liquidity`, and
  `/build/remove-liquidity` now return a prepared, simulated transaction
  (XDR + review summary) instead of a descriptive `{contract_id, function,
  args}` payload with a note to submit through Horizon. Request bodies gain
  a required `network` field (and `source_account` for swap).
- `ContractClient::new` now takes the full `PoolConfig` and the engine's
  `Network` instead of separate contract/token strings.

### Fixed
- The pool contract's instance-storage ledger key (used by `/pool/reserves`
  and friends) is now built with typed XDR instead of a hand-rolled byte
  buffer.
- **Pool-state reads decoded through official `stellar-xdr` types.** All
  remaining structural byte scanning is gone from production XDR paths:
  `/pool/reserves` decodes the pool contract's typed instance storage
  (`Reserve0`, `Reserve1`, `TimestampLast`, `LpToken`) via real
  `ScVal`/`ScMap` types instead of scanning raw bytes for `Reserve0` /
  `Reserve1` / `LpTotalSup` / `TimestampL` fragments; missing or
  type-mismatched fields are now errors rather than silently-guessed zeros.
- **LP balances and total supply now query the actual SEP-41 LP token
  contract** resolved from the pool's `DataKey::LpToken`, instead of
  reading an `"LpBalance"`-on-pool key that does not exist (which previously
  always returned `0`). `/pool/lp-balance` and `lp_total_supply` on
  `/pool/reserves` are functional.
- **LP-token keys are grouped through typed XDR** (`sepal41_balance_key` /
  `contract_persistent_ledger_key`) instead of the hand-assembled
  byte-offset + padding buffer previously used to build them.
- **Removed the hex/base64 contract-id fallback** (`parse_contract_id`);
  addresses and contract ids now go through full `stellar_strkey` checksum
  and type validation (`xdr::parse_address`), satisfying the criteria that
  contract ids and account addresses use full StrKey validation.
- Golden + mutation tests (`tests/pool_decode_test.rs`) prove malformed,
  truncated, wrong-type, and missing-key XDR fails safely, and that real
  ledger entries round-trip with correct typed values.

### Security
- **Breaking:** the engine no longer serves privileged endpoints to
  unauthenticated callers, and browsers can no longer call it cross-origin
  by default. See the "Service Authentication" section of the README and
  `.env.example` for the new required configuration.

