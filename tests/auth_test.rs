//! Contract tests for the Backend-to-Engine signed request protocol.
//!
//! These exercise `auth::require_scope` end-to-end through a real axum
//! router (not just the unit-level signature math in `src/auth.rs`), the
//! same way a Backend integration test would: build a signed request over
//! HTTP, send it through the middleware, and assert on the response.
//!
//! Covers: success, replay, tampering, clock skew, key rotation, and scope
//! escalation — the cases called out in the mainnet-readiness issue.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
    routing::get,
    Router,
};
use hmac::{Hmac, Mac};
use nodus_core_engine::auth::{self, AuthConfig, Scope};
use nodus_core_engine::config::Network;
use nodus_core_engine::nonce_store::MemoryNonceStore;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

type HmacSha256 = Hmac<Sha256>;

const KEY_ID: &str = "backend";
const SECRET_HEX: &str = "00112233445566778899aabbccddeeff";

fn one_key_state(clock_skew: Duration) -> auth::AuthState {
    let mut keys = HashMap::new();
    keys.insert(
        KEY_ID.to_string(),
        auth::parse_service_keys(&format!("{KEY_ID}:{SECRET_HEX}:read"))
            .unwrap()
            .remove(KEY_ID)
            .unwrap(),
    );
    Arc::new(AuthConfig {
        keys,
        network: Network::Testnet,
        clock_skew,
        replay_window: Duration::from_secs(300),
        nonces: Arc::new(MemoryNonceStore::new()),
    })
}

fn build_app(state: auth::AuthState) -> Router {
    Router::new()
        .route("/api/v1/rates", get(|| async { "ok" }))
        .route_layer(from_fn_with_state(
            (state, Scope::Read),
            auth::require_scope,
        ))
}

fn sign(secret_hex: &str, canonical: &str) -> String {
    let secret = hex::decode(secret_hex).unwrap();
    let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
    mac.update(canonical.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[allow(clippy::too_many_arguments)]
fn signed_request(
    method: &str,
    path: &str,
    body: &str,
    timestamp: i64,
    nonce: &str,
    scope: &str,
    network: &str,
    key_id: &str,
    secret_hex: &str,
) -> Request<Body> {
    let digest = hex::encode(Sha256::digest(body.as_bytes()));
    let canonical = format!("{method}\n{path}\n{digest}\n{timestamp}\n{nonce}\n{scope}\n{network}");
    let signature = sign(secret_hex, &canonical);

    Request::builder()
        .method(method)
        .uri(path)
        .header("x-nodus-key-id", key_id)
        .header("x-nodus-timestamp", timestamp.to_string())
        .header("x-nodus-nonce", nonce)
        .header("x-nodus-scope", scope)
        .header("x-nodus-network", network)
        .header("x-nodus-signature", signature)
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn success_valid_signature_is_accepted() {
    let app = build_app(one_key_state(Duration::from_secs(60)));
    let req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-ok",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn replay_same_nonce_twice_is_rejected_the_second_time() {
    let app = build_app(one_key_state(Duration::from_secs(60)));

    let first = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-replay",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res1 = app.clone().oneshot(first).await.unwrap();
    assert_eq!(res1.status(), StatusCode::OK);

    let second = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-replay",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res2 = app.oneshot(second).await.unwrap();
    assert_eq!(res2.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn tampering_with_signature_is_rejected() {
    let app = build_app(one_key_state(Duration::from_secs(60)));
    let mut req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-tamper",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    // Same request, forged signature — must not verify.
    req.headers_mut()
        .insert("x-nodus-signature", "00".repeat(32).parse().unwrap());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn clock_skew_outside_window_is_rejected() {
    let app = build_app(one_key_state(Duration::from_secs(30)));
    let stale_timestamp = now() - 3600; // an hour old, window is 30s
    let req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        stale_timestamp,
        "nonce-skew",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rotation_old_and_new_keys_both_work_until_old_is_removed() {
    let new_key_id = "backend-v2";
    let new_secret_hex = "aabbccddeeff00112233445566778899";

    let mut keys = HashMap::new();
    keys.insert(
        KEY_ID.to_string(),
        auth::parse_service_keys(&format!("{KEY_ID}:{SECRET_HEX}:read"))
            .unwrap()
            .remove(KEY_ID)
            .unwrap(),
    );
    keys.insert(
        new_key_id.to_string(),
        auth::parse_service_keys(&format!("{new_key_id}:{new_secret_hex}:read"))
            .unwrap()
            .remove(new_key_id)
            .unwrap(),
    );

    let state = Arc::new(AuthConfig {
        keys,
        network: Network::Testnet,
        clock_skew: Duration::from_secs(60),
        replay_window: Duration::from_secs(300),
        nonces: Arc::new(MemoryNonceStore::new()),
    });
    let app = build_app(state);

    let old_req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-old-key",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res_old = app.clone().oneshot(old_req).await.unwrap();
    assert_eq!(
        res_old.status(),
        StatusCode::OK,
        "old key must still work during rotation"
    );

    let new_req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-new-key",
        "read",
        "testnet",
        new_key_id,
        new_secret_hex,
    );
    let res_new = app.oneshot(new_req).await.unwrap();
    assert_eq!(
        res_new.status(),
        StatusCode::OK,
        "newly rotated-in key must work immediately"
    );
}

#[tokio::test]
async fn scope_escalation_signing_for_a_different_scope_is_rejected() {
    // The endpoint requires `read`; the caller signs for `tx_submit`
    // instead. This must fail even though the signature itself is valid
    // for the (wrong) scope it claims.
    let app = build_app(one_key_state(Duration::from_secs(60)));
    let req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-escalate",
        "tx_submit",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn key_not_granted_the_required_scope_is_rejected() {
    // Key only grants `read`, but we sign+claim `read` correctly while the
    // *key itself* isn't authorized for it (simulated by an empty grant).
    let mut keys = HashMap::new();
    keys.insert(
        KEY_ID.to_string(),
        auth::parse_service_keys(&format!("{KEY_ID}:{SECRET_HEX}:diagnostics"))
            .unwrap()
            .remove(KEY_ID)
            .unwrap(),
    );
    let state = Arc::new(AuthConfig {
        keys,
        network: Network::Testnet,
        clock_skew: Duration::from_secs(60),
        replay_window: Duration::from_secs(300),
        nonces: Arc::new(MemoryNonceStore::new()),
    });
    let app = build_app(state);

    let req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-not-granted",
        "read",
        "testnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wrong_network_is_rejected() {
    let app = build_app(one_key_state(Duration::from_secs(60)));
    let req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-network",
        "read",
        "mainnet",
        KEY_ID,
        SECRET_HEX,
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unknown_key_id_is_rejected() {
    let app = build_app(one_key_state(Duration::from_secs(60)));
    let req = signed_request(
        "GET",
        "/api/v1/rates",
        "",
        now(),
        "nonce-unknown",
        "read",
        "testnet",
        "nobody",
        SECRET_HEX,
    );
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_headers_are_rejected() {
    let app = build_app(one_key_state(Duration::from_secs(60)));
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/rates")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
