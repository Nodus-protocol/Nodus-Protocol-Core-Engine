//! Backend-to-Engine service authentication.
//!
//! Every non-health endpoint requires a signed request from a known
//! workload identity. The scheme binds the HTTP method, path, a digest of
//! the body, a timestamp, a caller-chosen nonce, the scope the caller is
//! invoking, and the target network into a single HMAC-SHA256 signature —
//! so a captured request cannot be replayed, tampered with, retargeted at a
//! different endpoint/scope, or forwarded to the wrong network.
//!
//! Callers send:
//!   X-Nodus-Key-Id:    the workload's key identifier (never a secret)
//!   X-Nodus-Timestamp: unix seconds when the request was signed
//!   X-Nodus-Nonce:     a unique-per-request opaque string (e.g. a UUID)
//!   X-Nodus-Scope:     the scope this request claims (must match the
//!                      endpoint's required scope and the key's grants)
//!   X-Nodus-Network:   "mainnet" or "testnet" — must match the engine's
//!                      configured network
//!   X-Nodus-Signature: hex(HMAC-SHA256(secret, canonical_string))
//!
//! canonical_string = "{METHOD}\n{PATH}\n{sha256_hex(body)}\n{timestamp}\n{nonce}\n{scope}\n{network}"
//!
//! Signature verification runs as an axum middleware layered per-route (or
//! per route-group) via [`require_scope`], so each endpoint declares the
//! scope it needs and the middleware rejects anything else before the
//! handler ever runs.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::Network;

type HmacSha256 = Hmac<Sha256>;

/// Max body size read while computing the signature digest. Requests larger
/// than this are rejected outright — the body size limit layer in `main.rs`
/// enforces the same ceiling for non-authenticated bytes-on-the-wire cost.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

// ── Scopes ───────────────────────────────────────────────────────────────────

/// Endpoint risk tiers. A key is only authorized for the scopes explicitly
/// granted to it; a signature is only valid for the scope it was signed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read-only endpoints: quotes, balances, payment/webhook listings.
    Read,
    /// Builds an unsigned transaction envelope; does not touch the chain.
    TxConstruct,
    /// Submits a transaction / payment for on-chain settlement.
    TxSubmit,
    /// Administrative endpoints (webhook registration, key/config changes).
    Admin,
    /// Diagnostic endpoints beyond the always-open health probe.
    Diagnostics,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::TxConstruct => "tx_construct",
            Scope::TxSubmit => "tx_submit",
            Scope::Admin => "admin",
            Scope::Diagnostics => "diagnostics",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Scope::Read),
            "tx_construct" => Some(Scope::TxConstruct),
            "tx_submit" => Some(Scope::TxSubmit),
            "admin" => Some(Scope::Admin),
            "diagnostics" => Some(Scope::Diagnostics),
            _ => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Service key registry ────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ServiceKey {
    pub secret: Vec<u8>,
    pub scopes: Vec<Scope>,
}

/// The authenticated caller, attached to request extensions after a
/// successful verification. Handlers/log statements may read `key_id` but
/// the secret never leaves this module.
#[derive(Debug, Clone)]
pub struct ServiceIdentity {
    pub key_id: String,
}

/// Parses the `ENGINE_AUTH_KEYS` env format:
///   `key_id:hex_secret:scope1|scope2;key_id2:hex_secret2:scope1`
///
/// Multiple keys may be active simultaneously, which is what makes secret
/// rotation zero-downtime: add the new key, roll callers over, then remove
/// the old entry in a later deploy.
pub fn parse_service_keys(raw: &str) -> Result<HashMap<String, ServiceKey>, String> {
    let mut keys = HashMap::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut parts = entry.splitn(3, ':');
        let key_id = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("malformed key entry (missing key_id): {entry}"))?;
        let secret_hex = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("malformed key entry (missing secret): {entry}"))?;
        let scopes_raw = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("malformed key entry (missing scopes): {entry}"))?;

        let secret = hex::decode(secret_hex)
            .map_err(|e| format!("key {key_id}: secret must be hex-encoded: {e}"))?;
        if secret.len() < 16 {
            return Err(format!(
                "key {key_id}: secret too short (>=16 bytes required, got {})",
                secret.len()
            ));
        }

        let scopes: Vec<Scope> = scopes_raw
            .split('|')
            .map(|s| Scope::parse(s).ok_or_else(|| format!("key {key_id}: unknown scope '{s}'")))
            .collect::<Result<_, _>>()?;
        if scopes.is_empty() {
            return Err(format!("key {key_id}: no scopes granted"));
        }

        keys.insert(key_id.to_string(), ServiceKey { secret, scopes });
    }
    Ok(keys)
}

// ── Auth state / middleware ─────────────────────────────────────────────────

pub struct AuthConfig {
    pub keys: HashMap<String, ServiceKey>,
    pub network: Network,
    pub clock_skew: Duration,
    pub replay_window: Duration,
    pub nonces: Arc<dyn crate::nonce_store::NonceStore>,
}

pub type AuthState = Arc<AuthConfig>;

#[derive(Debug, Serialize)]
struct AuthError {
    code: &'static str,
    message: String,
}

fn reject(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(AuthError {
            code,
            message: message.into(),
        }),
    )
        .into_response()
}

/// Axum middleware factory: apply per-route with
/// `.route_layer(from_fn_with_state((auth_state.clone(), Scope::Read), auth::require_scope))`.
pub async fn require_scope(
    State((auth, required_scope)): State<(AuthState, Scope)>,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();

    let key_id = match header_str(&parts.headers, "x-nodus-key-id") {
        Some(v) => v,
        None => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "MISSING_KEY_ID",
                "missing X-Nodus-Key-Id",
            )
        }
    };
    let timestamp_raw = match header_str(&parts.headers, "x-nodus-timestamp") {
        Some(v) => v,
        None => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "MISSING_TIMESTAMP",
                "missing X-Nodus-Timestamp",
            )
        }
    };
    let nonce = match header_str(&parts.headers, "x-nodus-nonce") {
        Some(v) => v,
        None => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "MISSING_NONCE",
                "missing X-Nodus-Nonce",
            )
        }
    };
    let claimed_scope_raw = match header_str(&parts.headers, "x-nodus-scope") {
        Some(v) => v,
        None => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "MISSING_SCOPE",
                "missing X-Nodus-Scope",
            )
        }
    };
    let network_raw = match header_str(&parts.headers, "x-nodus-network") {
        Some(v) => v,
        None => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "MISSING_NETWORK",
                "missing X-Nodus-Network",
            )
        }
    };
    let signature_hex = match header_str(&parts.headers, "x-nodus-signature") {
        Some(v) => v,
        None => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "MISSING_SIGNATURE",
                "missing X-Nodus-Signature",
            )
        }
    };

    // The signed scope must be exactly the scope this route requires — this
    // is what stops a signature captured for a low-privilege endpoint from
    // being replayed against a higher-privilege one.
    if claimed_scope_raw != required_scope.as_str() {
        return reject(
            StatusCode::FORBIDDEN,
            "SCOPE_MISMATCH",
            format!(
                "request signed for scope '{claimed_scope_raw}' cannot be used on a '{required_scope}' endpoint"
            ),
        );
    }

    let key = match auth.keys.get(&key_id) {
        Some(k) => k,
        None => return reject(StatusCode::UNAUTHORIZED, "UNKNOWN_KEY", "unknown key id"),
    };
    if !key.scopes.contains(&required_scope) {
        return reject(
            StatusCode::FORBIDDEN,
            "SCOPE_NOT_GRANTED",
            format!("key is not authorized for scope '{required_scope}'"),
        );
    }

    let claimed_network = match network_raw.as_str() {
        "mainnet" => Network::Mainnet,
        "testnet" => Network::Testnet,
        _ => {
            return reject(
                StatusCode::BAD_REQUEST,
                "INVALID_NETWORK",
                "X-Nodus-Network must be 'mainnet' or 'testnet'",
            )
        }
    };
    if claimed_network != auth.network {
        return reject(
            StatusCode::FORBIDDEN,
            "NETWORK_MISMATCH",
            "request signed for a different network than this deployment",
        );
    }

    let timestamp: i64 = match timestamp_raw.parse() {
        Ok(v) => v,
        Err(_) => {
            return reject(
                StatusCode::BAD_REQUEST,
                "INVALID_TIMESTAMP",
                "X-Nodus-Timestamp must be a unix-seconds integer",
            )
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let skew = auth.clock_skew.as_secs() as i64;
    if (now - timestamp).abs() > skew {
        return reject(
            StatusCode::UNAUTHORIZED,
            "CLOCK_SKEW",
            "timestamp outside the accepted clock-skew window",
        );
    }

    if nonce.len() < 8 || nonce.len() > 128 {
        return reject(
            StatusCode::BAD_REQUEST,
            "INVALID_NONCE",
            "nonce must be between 8 and 128 characters",
        );
    }

    let body_bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return reject(
                StatusCode::PAYLOAD_TOO_LARGE,
                "BODY_TOO_LARGE",
                "request body exceeds the signable size limit",
            )
        }
    };
    let body_digest = hex::encode(Sha256::digest(body_bytes.as_ref()));

    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}",
        parts.method.as_str(),
        parts.uri.path(),
        body_digest,
        timestamp_raw,
        nonce,
        claimed_scope_raw,
        network_raw,
    );

    let sig_bytes = match hex::decode(&signature_hex) {
        Ok(b) => b,
        Err(_) => {
            return reject(
                StatusCode::UNAUTHORIZED,
                "INVALID_SIGNATURE_ENCODING",
                "X-Nodus-Signature must be hex-encoded",
            )
        }
    };

    let mut mac = match HmacSha256::new_from_slice(&key.secret) {
        Ok(m) => m,
        Err(_) => return reject(StatusCode::INTERNAL_SERVER_ERROR, "KEY_ERROR", "key error"),
    };
    mac.update(canonical.as_bytes());
    if mac.verify_slice(&sig_bytes).is_err() {
        return reject(
            StatusCode::UNAUTHORIZED,
            "BAD_SIGNATURE",
            "signature verification failed",
        );
    }

    // Only record the nonce as spent *after* the signature is valid — an
    // attacker who cannot forge a signature should not be able to burn a
    // legitimate caller's nonce via a garbage request.
    let replay_key = format!("{key_id}:{nonce}");
    match auth
        .nonces
        .check_and_set(&replay_key, auth.replay_window)
        .await
    {
        Ok(true) => {
            return reject(
                StatusCode::CONFLICT,
                "REPLAYED_NONCE",
                "this nonce has already been used",
            )
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "nonce store error");
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "NONCE_STORE_UNAVAILABLE",
                "replay-protection store unavailable",
            );
        }
    }

    tracing::info!(workload = %key_id, scope = %required_scope, "authenticated request");

    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    req.extensions_mut().insert(ServiceIdentity { key_id });
    next.run(req).await
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], canonical: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(canonical.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn parses_single_key() {
        let keys =
            parse_service_keys("backend:00112233445566778899aabbccddeeff:read|tx_submit").unwrap();
        let key = keys.get("backend").unwrap();
        assert_eq!(key.scopes, vec![Scope::Read, Scope::TxSubmit]);
    }

    #[test]
    fn parses_multiple_keys_for_rotation() {
        let keys = parse_service_keys(
            "old:00112233445566778899aabbccddeeff:read;new:aabbccddeeff00112233445566778899:read",
        )
        .unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains_key("old"));
        assert!(keys.contains_key("new"));
    }

    #[test]
    fn rejects_short_secret() {
        assert!(parse_service_keys("k:aabb:read").is_err());
    }

    #[test]
    fn rejects_unknown_scope() {
        assert!(parse_service_keys("k:00112233445566778899aabbccddeeff:frobnicate").is_err());
    }

    #[test]
    fn signature_round_trips() {
        let secret = b"a-sixteen-byte-plus-secret-key!!";
        let canonical = "GET\n/api/v1/rates\nabc123\n1700000000\nnonce-1\nread\ntestnet";
        let sig = sign(secret, canonical);

        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(canonical.as_bytes());
        assert!(mac.verify_slice(&hex::decode(sig).unwrap()).is_ok());
    }

    #[test]
    fn tampered_body_breaks_signature() {
        let secret = b"a-sixteen-byte-plus-secret-key!!";
        let original = "POST\n/api/v1/payments\ndigest-a\n1700000000\nnonce-1\ntx_submit\ntestnet";
        let tampered = "POST\n/api/v1/payments\ndigest-b\n1700000000\nnonce-1\ntx_submit\ntestnet";
        let sig = sign(secret, original);

        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(tampered.as_bytes());
        assert!(mac.verify_slice(&hex::decode(sig).unwrap()).is_err());
    }
}
