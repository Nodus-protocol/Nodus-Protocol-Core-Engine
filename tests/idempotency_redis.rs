//! Concurrency and failure coverage for the atomic idempotency protocol
//! against a **real Redis** — the Lua scripts, not the in-memory model.
//!
//! Every test here is `#[ignore]` so a plain `cargo test` needs no Redis. CI
//! runs them with `cargo test --test idempotency_redis -- --ignored` against
//! the `redis` service container, with `NODUS_TEST_REDIS_URL` set. Run
//! locally with:
//!
//! ```sh
//! docker run --rm -d -p 6379:6379 redis:7
//! NODUS_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo test --test idempotency_redis -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use nodus_core_engine::idempotency::{ClaimOutcome, ClaimStore, RedisClaimStore};
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(30);
const LEASE_DEAD: Duration = Duration::from_millis(0);
const TTL: Duration = Duration::from_secs(3_600);

async fn store() -> RedisClaimStore {
    let url = std::env::var("NODUS_TEST_REDIS_URL")
        .expect("set NODUS_TEST_REDIS_URL to run the real-Redis idempotency suite");
    RedisClaimStore::new(&url)
        .await
        .expect("connect to NODUS_TEST_REDIS_URL")
}

/// A fresh key per test so a shared Redis instance is safe to reuse.
fn key(tag: &str) -> String {
    format!(
        "nodus:idem:test:mainnet:payments.initiate:{tag}:{}",
        Uuid::new_v4()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires NODUS_TEST_REDIS_URL"]
async fn concurrent_claims_across_tasks_grant_exactly_one() {
    let store = Arc::new(store().await);
    let key = key("race");

    let mut handles = Vec::new();
    for i in 0..48 {
        let store = store.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            store
                .claim(&key, "fp", &format!("owner-{i}"), LEASE, TTL)
                .await
                .unwrap()
        }));
    }

    let mut granted = 0usize;
    for h in handles {
        match h.await.unwrap() {
            ClaimOutcome::Claimed(_) => granted += 1,
            ClaimOutcome::InFlight => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert_eq!(
        granted, 1,
        "the Lua claim script must grant exactly one owner"
    );
}

#[tokio::test]
#[ignore = "requires NODUS_TEST_REDIS_URL"]
async fn completed_response_is_replayed_verbatim() {
    let store = store().await;
    let key = key("replay");
    let response = serde_json::json!({"id": "p-redis", "nested": {"b": 2, "a": 1}});

    let token = match store.claim(&key, "fp", "o1", LEASE, TTL).await.unwrap() {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };
    store
        .complete(&token, &response, Some("tx-1"), TTL)
        .await
        .unwrap();

    match store.claim(&key, "fp", "o2", LEASE, TTL).await.unwrap() {
        ClaimOutcome::Replay { response: r } => assert_eq!(r, response),
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires NODUS_TEST_REDIS_URL"]
async fn fingerprint_mismatch_is_rejected() {
    let store = store().await;
    let key = key("mismatch");
    store.claim(&key, "fp-a", "o", LEASE, TTL).await.unwrap();
    let err = store
        .claim(&key, "fp-b", "o", LEASE, TTL)
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 409);
}

#[tokio::test]
#[ignore = "requires NODUS_TEST_REDIS_URL"]
async fn a_submitting_claim_is_never_taken_over() {
    let store = store().await;
    let key = key("submitting");

    let token = match store
        .claim(&key, "fp", "o1", LEASE_DEAD, TTL)
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };
    store
        .mark_submitting(&token, "chain-tx-1", LEASE_DEAD, TTL)
        .await
        .unwrap();

    match store.claim(&key, "fp", "o2", LEASE, TTL).await.unwrap() {
        ClaimOutcome::AwaitingResult { execution_ref } => assert_eq!(execution_ref, "chain-tx-1"),
        other => panic!("a submitting claim must yield AwaitingResult; got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires NODUS_TEST_REDIS_URL"]
async fn expired_in_flight_lease_is_taken_over() {
    let store = store().await;
    let key = key("takeover");

    store
        .claim(&key, "fp", "dead", LEASE_DEAD, TTL)
        .await
        .unwrap();
    match store.claim(&key, "fp", "alive", LEASE, TTL).await.unwrap() {
        ClaimOutcome::TookOver(_) => {}
        other => panic!("expected TookOver, got {other:?}"),
    }
}

/// The `ConnectionManager` reconnects transparently, so a connection gap
/// (Redis restart, network blip) between operations must not corrupt a claim.
/// A true kill/restart needs container control; this exercises the same code
/// path by spacing operations out and asserting end-to-end consistency.
#[tokio::test]
#[ignore = "requires NODUS_TEST_REDIS_URL"]
async fn claim_survives_gaps_between_operations() {
    let store = store().await;
    let key = key("reconnect");

    let token = match store.claim(&key, "fp", "o1", LEASE, TTL).await.unwrap() {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    store
        .mark_submitting(&token, "tx-recon", LEASE, TTL)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    store
        .complete(
            &token,
            &serde_json::json!({"ok": true}),
            Some("tx-recon"),
            TTL,
        )
        .await
        .unwrap();

    match store.claim(&key, "fp", "o2", LEASE, TTL).await.unwrap() {
        ClaimOutcome::Replay { response } => assert_eq!(response, serde_json::json!({"ok": true})),
        other => panic!("expected Replay, got {other:?}"),
    }
}
