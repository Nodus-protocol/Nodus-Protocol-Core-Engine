//! Cross-instance concurrency proof for the atomic idempotency protocol.
//!
//! Many identical requests share one [`MemoryClaimStore`] (standing in for N
//! engine instances pointed at one store) and race to claim the same key.
//! Exactly one must be granted execution; every other must be told to wait or
//! replay. The equivalent proof against a real Redis store lives in
//! `idempotency_redis.rs` and runs in CI against the `redis` service.

use std::sync::Arc;
use std::time::Duration;

use nodus_core_engine::idempotency::{ClaimOutcome, ClaimStore, MemoryClaimStore};

const LEASE: Duration = Duration::from_secs(30);
const TTL: Duration = Duration::from_secs(86_400);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_identical_requests_grant_exactly_one_execution() {
    let store = Arc::new(MemoryClaimStore::new());
    let key = "nodus:idem:test:mainnet:payments.initiate:race-1";

    let mut handles = Vec::new();
    for i in 0..64 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .claim(key, "fp", &format!("owner-{i}"), LEASE, TTL)
                .await
                .unwrap()
        }));
    }

    let mut granted = 0usize;
    let mut waiting = 0usize;
    for h in handles {
        match h.await.unwrap() {
            ClaimOutcome::Claimed(_) => granted += 1,
            ClaimOutcome::InFlight => waiting += 1,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    assert_eq!(granted, 1, "exactly one request may execute");
    assert_eq!(waiting, 63, "every other request must wait");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn once_completed_every_later_request_replays_the_same_response() {
    let store = Arc::new(MemoryClaimStore::new());
    let key = "nodus:idem:test:mainnet:payments.initiate:race-2";
    let response = serde_json::json!({"id": "the-one-payment", "status": "confirmed"});

    let token = match store.claim(key, "fp", "winner", LEASE, TTL).await.unwrap() {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };
    store
        .complete(&token, &response, Some("tx-1"), TTL)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..32 {
        let store = store.clone();
        let expected = response.clone();
        handles.push(tokio::spawn(async move {
            match store
                .claim(key, "fp", &format!("late-{i}"), LEASE, TTL)
                .await
                .unwrap()
            {
                ClaimOutcome::Replay { response } => assert_eq!(response, expected),
                other => panic!("expected Replay, got {other:?}"),
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn mismatched_payload_under_one_key_is_rejected() {
    let store = MemoryClaimStore::new();
    let key = "nodus:idem:test:mainnet:payments.initiate:mismatch";

    store
        .claim(key, "fingerprint-A", "o", LEASE, TTL)
        .await
        .unwrap();
    let err = store
        .claim(key, "fingerprint-B", "o", LEASE, TTL)
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 409);
}
