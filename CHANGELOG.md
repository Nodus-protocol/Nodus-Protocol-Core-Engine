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

### Security
- **Breaking:** the engine no longer serves privileged endpoints to
  unauthenticated callers, and browsers can no longer call it cross-origin
  by default. See the "Service Authentication" section of the README and
  `.env.example` for the new required configuration.

