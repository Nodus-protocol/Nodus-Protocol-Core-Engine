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

### Known limitations
- `/pool/lp-balance` and the `lp_total_supply` field on `/pool/reserves`
  still read a key that no longer exists on the pool contract's own
  storage — LP balances and total supply now live on a separate SEP-41 LP
  token contract per the deployed contract's source. Pre-existing behavior,
  left as-is and out of scope for this change; see `ContractClient::lp_balance`.

### Security
- **Breaking:** the engine no longer serves privileged endpoints to
  unauthenticated callers, and browsers can no longer call it cross-origin
  by default. See the "Service Authentication" section of the README and
  `.env.example` for the new required configuration.

