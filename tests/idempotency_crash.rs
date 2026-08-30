//! Crash-scenario coverage for the atomic idempotency protocol.
//!
//! A "crash" here is an owner that acquires a claim and then simply stops —
//! it never calls `complete`, and its lease expires. A successor must resolve
//! the claim without ever duplicating an irreversible submission.

use std::time::Duration;

use nodus_core_engine::idempotency::{ClaimOutcome, ClaimStore, MemoryClaimStore};

const LEASE_LONG: Duration = Duration::from_secs(30);
const LEASE_DEAD: Duration = Duration::from_millis(0);
const TTL: Duration = Duration::from_secs(86_400);

/// claim-before-work: the owner claimed but never performed any side effect,
/// then died. A successor takes over and runs the work exactly once.
#[tokio::test]
async fn claim_before_work_crash_is_safely_taken_over() {
    let store = MemoryClaimStore::new();
    let key = "k-claim-before-work";

    // Owner A claims with an already-expired lease, then "crashes" (drops the
    // token without completing).
    let _dead = match store
        .claim(key, "fp", "owner-a", LEASE_DEAD, TTL)
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };

    // Owner B takes over — the claim never left InFlight, so re-execution is safe.
    let token_b = match store
        .claim(key, "fp", "owner-b", LEASE_LONG, TTL)
        .await
        .unwrap()
    {
        ClaimOutcome::TookOver(t) => t,
        other => panic!("expected TookOver, got {other:?}"),
    };
    let response = serde_json::json!({"id": "p-b"});
    store
        .complete(&token_b, &response, Some("tx-b"), TTL)
        .await
        .unwrap();

    match store
        .claim(key, "fp", "owner-c", LEASE_LONG, TTL)
        .await
        .unwrap()
    {
        ClaimOutcome::Replay { response: r } => assert_eq!(r, response),
        other => panic!("expected Replay, got {other:?}"),
    }
}

/// submission-before-result: the owner recorded an execution reference and
/// then died before the outcome was known. A successor must NOT re-submit —
/// it is handed the reference to reconcile.
#[tokio::test]
async fn submission_before_result_crash_is_not_resubmitted() {
    let store = MemoryClaimStore::new();
    let key = "k-submission-before-result";

    let token_a = match store
        .claim(key, "fp", "owner-a", LEASE_DEAD, TTL)
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };
    store
        .mark_submitting(&token_a, "chain-tx-0xdead", LEASE_DEAD, TTL)
        .await
        .unwrap();
    // owner A crashes here — no complete().

    for owner in ["owner-b", "owner-c", "owner-d"] {
        match store
            .claim(key, "fp", owner, LEASE_LONG, TTL)
            .await
            .unwrap()
        {
            ClaimOutcome::AwaitingResult { execution_ref } => {
                assert_eq!(execution_ref, "chain-tx-0xdead");
            }
            other => panic!("a submitting claim must never be re-granted; got {other:?}"),
        }
    }
}

/// A stale owner that returns after being taken over cannot commit a result.
#[tokio::test]
async fn stale_owner_cannot_complete_after_takeover() {
    let store = MemoryClaimStore::new();
    let key = "k-stale-complete";

    let stale = match store
        .claim(key, "fp", "owner-a", LEASE_DEAD, TTL)
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed(t) => t,
        other => panic!("expected Claimed, got {other:?}"),
    };
    // Successor takes over.
    let _fresh = store
        .claim(key, "fp", "owner-b", LEASE_LONG, TTL)
        .await
        .unwrap();

    let err = store
        .complete(&stale, &serde_json::json!({"id": "stale"}), None, TTL)
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 409);
}
