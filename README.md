<div align="center">

<h1>Nodus Protocol — Core Engine</h1>

<p>The payment processing backbone of the Nodus Protocol ecosystem.<br/>Fast, composable, and built for the decentralized web.</p>

[![CI](https://github.com/Nodus-protocol/Nodus-Protocol-Core-Engine/actions/workflows/ci.yml/badge.svg)](https://github.com/Nodus-protocol/Nodus-Protocol-Core-Engine/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-violet.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.82-orange?logo=rust)](Cargo.toml)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](CONTRIBUTING.md)

</div>

---

## What is the Core Engine?

The **Nodus Protocol Core Engine** is the settlement and routing layer that powers seamless, permissionless payments across the Nodus ecosystem. It abstracts away the complexity of multi-chain transactions so that users and developers can move value as easily as sending a message.

Whether you're building a checkout flow, a subscription service, or a cross-chain payment app, the Core Engine handles the heavy lifting — routing, validation, settlement, and confirmation.

---

## Features

- **One-click payments** — Customers initiate transfers without managing gas, bridges, or slippage manually.
- **Multi-chain routing** — Automatically selects the optimal path across supported Substrate networks (Aleph Zero, Astar, Shiden) to minimize cost and latency.
- **Instant settlement** — Transactions are confirmed and settled in seconds, not minutes.
- **Non-custodial** — The engine never holds user funds; all transfers go directly between parties.
- **Composable** — Drop the engine into any stack via a clean API and SDK.
- **Auditable** — Every payment produces an on-chain receipt, queryable at any time.

---

## How It Works

```
Customer initiates payment
        │
        ▼
 Core Engine receives request
        │
        ├─ Validates sender & recipient
        ├─ Selects optimal chain route
        ├─ Estimates & abstracts fees
        │
        ▼
 Transaction submitted on-chain
        │
        ▼
 Settlement confirmed + receipt emitted
        │
        ▼
 Merchant/recipient notified
```

---

## Getting Started

### Prerequisites

- Rust 1.80+ (2024 edition)
- Cargo
- An RPC endpoint for your target network

### Installation

```bash
git clone https://github.com/Nodus-protocol/Nodus-Protocol-Core-Engine.git
cd Nodus-Protocol-Core-Engine
cargo build
```

### Configuration

Copy the example environment file and fill in your values:

```bash
cp .env.example .env
```

| Variable | Description |
|---|---|
| `RPC_URL` | Substrate RPC endpoint for the target chain (e.g. Aleph Zero, Astar) |
| `PRIVATE_KEY` | SR25519 signing key (SS58 format) for the engine wallet |
| `SETTLEMENT_CONTRACT` | SS58 address of the deployed LiquidityPool contract |
| `NETWORK` | Target network (`mainnet`, `testnet`) |

### Running locally

```bash
cargo run
```

### Running tests

```bash
cargo test
```

---

## API Overview

### Initiate a payment

```http
POST /api/v1/pay
Content-Type: application/json

{
  "from": "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY",
  "to": "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty",
  "amount": "50.00",
  "currency": "USDC",
  "network": "aleph-zero"
}
```

**Response**

```json
{
  "status": "confirmed",
  "txHash": "0xabc123...",
  "settledAt": "2025-01-01T12:00:00Z",
  "fee": "0.001 USDC",
  "receipt": "ipfs://Qm..."
}
```

### Query a payment

```http
GET /api/v1/pay/:txHash
```

### Supported tokens

| Symbol | Network |
|---|---|
| AZERO | Aleph Zero |
| USDC | Aleph Zero (PSP22) |
| USDT | Aleph Zero (PSP22) |
| DOT | Astar, Shiden |
| ASTR | Astar |

---

## AMM Pool — Soroban Transaction Preparation

Pool write calls (`swap`, `add_liquidity`, `remove_liquidity`) are prepared,
not blindly forwarded: the engine encodes real, typed Soroban XDR against an
embedded manifest of the pool contract's audited ABI (see
[`src/pool/abi.rs`](src/pool/abi.rs)), simulates it against Soroban RPC, and
hands back the fully-prepared transaction alongside a review summary decoded
straight back out of that same XDR — never from the original request — so
what you're shown is provably what you'd be signing.

```http
POST /api/v1/pool/build/swap
Content-Type: application/json

{
  "network": "testnet",
  "source_account": "GABC...",
  "to": "GABC...",
  "amount_0_out": 1000000,
  "amount_1_out": 0,
  "deadline": 1735689600
}
```

**Response**

```json
{
  "xdr": "AAAAAgAAAAA...",
  "review": {
    "spec_hash": "5393928...",
    "contract": "CABC...",
    "function": "swap",
    "args": { "to": "GABC...", "amount_0_out": "1000000", "amount_1_out": "0", "deadline": "1735689600" },
    "source_account": "GABC...",
    "sequence": 12345,
    "fee_stroops": 1100,
    "resource_fee_stroops": 1000,
    "deadline": 1735689600,
    "operation_count": 1,
    "auth_entry_count": 0,
    "auth": []
  }
}
```

Preparation binds network, source account, current sequence, contract,
exact amounts, recipient, deadline, and base fee; it fails closed on an
expired deadline, a fee above `POOL_FEE_CEILING_STROOPS`, an unfunded
source account, a simulation error, or ledger entries that need
restoration. **Frontend and Mobile should independently decode and verify
the returned `xdr` before requesting a signature** — don't trust `review`
alone.

`add_liquidity` and `remove_liquidity` work the same way at
`/api/v1/pool/build/add-liquidity` and `/api/v1/pool/build/remove-liquidity`.

### Validating a transaction

`POST /api/v1/pool/validate` decodes and policy-checks *any* prepared
transaction XDR — the same checks preparation runs on its own output —
useful for verifying a transaction you didn't build yourself:

```http
POST /api/v1/pool/validate
Content-Type: application/json

{ "xdr": "AAAAAgAAAAA...", "network": "testnet", "source_account": "GABC..." }
```

### Submitting a transaction

Preparation never signs anything — that's Frontend/Mobile's job. Once
signed, a caller can submit the XDR directly via their own Soroban RPC
access, or ask the engine to own submission instead:

```http
POST /api/v1/pool/submit
Content-Type: application/json

{ "signed_xdr": "AAAAAgAAAAA..." }
```

This relays to Soroban RPC's `sendTransaction` and polls `getTransaction`
until it resolves. Ownership is explicit per endpoint: calling `/submit`
means the engine owns submission for that transaction; not calling it means
you do.

---

## SDK

A JavaScript/TypeScript SDK is available for easy integration:

```ts
import { NodusEngine } from "@nodus/core-engine"

const engine = new NodusEngine({ network: "aleph-zero" })

const receipt = await engine.pay({
  from: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY",
  to: "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty",
  amount: "100",
  currency: "USDC",
})

console.log(receipt.txHash)
```

---

## Project Structure

```
Nodus-Protocol-Core-Engine/
├── src/
│   ├── engine/         # Core payment routing & settlement logic
│   ├── adapters/       # Chain-specific adapters (EVM, etc.)
│   ├── api/            # REST API handlers
│   └── utils/          # Helpers, fee estimation, validation
├── contracts/          # On-chain settlement contracts
├── tests/              # Unit and integration tests
└── docs/               # Extended documentation
```

---

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

1. Fork the repo
2. Create your branch: `git checkout -b feat/your-feature`
3. Commit your changes with a clear message
4. Push to your fork and open a PR against `main`

---

## Service Authentication

Every endpoint except `/healthz`, `/readyz`, and `/metrics` requires a signed
request from a known workload identity — there is no anonymous or IP-based
trust, even inside a private network. Those three stay open because they
carry no privileged data (a liveness probe, a readiness summary, and
Prometheus-style counters — see `src/observability.rs`) and load balancers
and scrapers need to reach them without a signing key. This is the
Backend-to-Engine protocol; see [`src/auth.rs`](src/auth.rs) for the
reference implementation.

### Signing a request

Send these headers on every call:

| Header | Value |
|---|---|
| `X-Nodus-Key-Id` | Your workload's key identifier |
| `X-Nodus-Timestamp` | Unix seconds when the request was signed |
| `X-Nodus-Nonce` | A unique-per-request opaque string (e.g. a UUID) |
| `X-Nodus-Scope` | The scope this request claims — must match the endpoint |
| `X-Nodus-Network` | `mainnet` or `testnet` — must match the engine's deployment |
| `X-Nodus-Signature` | `hex(HMAC-SHA256(secret, canonical_string))` |

```
canonical_string = "{METHOD}\n{PATH}\n{sha256_hex(body)}\n{timestamp}\n{nonce}\n{scope}\n{network}"
```

`PATH` is the request path with no query string. `sha256_hex(body)` is the
hex-encoded SHA-256 digest of the raw request body (`sha256("")` for a
request with no body). Binding method, path, body digest, timestamp, nonce,
scope, and network into one signature means a captured request cannot be
replayed, tampered with, retargeted at a different endpoint, escalated to a
higher-privilege scope, or forwarded to the wrong network — each of those
changes the canonical string and invalidates the signature.

### Scopes

| Scope | Covers |
|---|---|
| `read` | Quotes, balances, payment/webhook listings |
| `tx_construct` | Building an unsigned transaction envelope; no chain effect |
| `tx_submit` | Submitting a payment or transaction for settlement |
| `admin` | Webhook subscription management |
| `diagnostics` | Reserved for future debug endpoints beyond `/healthz` |

A key only grants what it needs (see `ENGINE_AUTH_KEYS` in `.env.example`).
A signature is only valid for the single scope it was signed for, and the
engine rejects it outright if the scope doesn't match what the target
endpoint requires.

### Replay protection

Every `(key_id, nonce)` pair is recorded durably (Redis when `REDIS_URL` is
set, in-memory otherwise) for `ENGINE_AUTH_REPLAY_WINDOW_SECS`. A repeated
nonce is rejected with `409 Conflict`. Timestamps outside
`ENGINE_AUTH_CLOCK_SKEW_SECS` of the engine's clock are rejected with `401`
regardless of nonce state.

### Rotating credentials without downtime

`ENGINE_AUTH_KEYS` accepts multiple active key entries at once. To rotate a
secret: add a new `key_id:secret:scopes` entry, redeploy, switch the caller
over to the new key, then remove the old entry in a later deploy. Both keys
work simultaneously in between — no restart-induced auth outage.

### CORS

`CORS_ALLOWED_ORIGINS` is an explicit comma-separated origin allow-list.
Leave it unset for an internal-only deployment: with no origins configured
the engine attaches no CORS layer at all, so browsers refuse cross-origin
responses outright (no permissive `*` wildcard is ever used). Set it only
when a browser genuinely needs to call the engine directly.

### Transport

The engine speaks plain HTTP itself and expects TLS to be terminated in
front of it (load balancer, ingress, or a service-mesh mTLS sidecar). Set
`TLS_TERMINATED_UPSTREAM=true` once that's in place — the engine refuses to
start on `NETWORK=mainnet` without it (`ENGINE_ALLOW_INSECURE_TRANSPORT=true`
is a local-dev-only override).

### Rate, size, and concurrency limits

Requests are capped per authenticated workload and scope (stricter for
`tx_submit`/`admin` than `read`), bodies are capped at 256 KiB, and each
scope group has its own concurrency ceiling — so a burst against one
endpoint tier can't starve another.

### Local development

Set `ENGINE_AUTH_DISABLED=true` to skip signature verification (testnet
only — the engine refuses to start with this set on `NETWORK=mainnet`).

---

## Security

If you discover a vulnerability, please **do not** open a public issue. Contact the team privately at **security@nodusprotocol.io**.

---

## License

[MIT](LICENSE) © Nodus Protocol
