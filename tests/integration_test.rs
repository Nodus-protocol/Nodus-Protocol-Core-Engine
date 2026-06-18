use std::sync::Arc;
use std::time::Duration;

use nodus_core_engine::adapters::mock::MockAdapter;
use nodus_core_engine::engine::Engine;
use nodus_core_engine::idempotency::MemoryIdempotencyStore;
use nodus_core_engine::retry::RetryConfig;
use nodus_core_engine::utils::{PaymentStatus, Urgency};

fn memory_store() -> Arc<MemoryIdempotencyStore> {
    Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(86_400)))
}

fn engine_with_mock(mock: MockAdapter) -> Engine {
    Engine::new(
        vec![Arc::new(mock)],
        RetryConfig::new(1, 0),
        memory_store(),
    )
}

const ALICE: &str = "GAHJJJKMOKYE4RVPZEWZTKH5FVI4PA3VL7GK2LFNUBSGBV7REEX6XCLD";
const BOB: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

#[tokio::test]
async fn successful_payment_is_confirmed() {
    let engine = engine_with_mock(MockAdapter::new("mock"));

    let payment = engine
        .initiate(
            ALICE.into(),
            BOB.into(),
            1_000_000,
            "XLM".into(),
            Urgency::Standard,
        )
        .await
        .expect("initiate failed");

    assert_eq!(payment.status, PaymentStatus::Confirmed);
    assert!(payment.tx_hash.is_some());
    assert_eq!(payment.sender, ALICE);
    assert_eq!(payment.recipient, BOB);
    assert_eq!(payment.amount, 1_000_000);
}

#[tokio::test]
async fn failed_adapter_marks_payment_failed() {
    let engine = engine_with_mock(MockAdapter::failing("mock-fail"));

    let payment = engine
        .initiate(ALICE.into(), BOB.into(), 500, "XLM".into(), Urgency::Fast)
        .await
        .expect("initiate should not error at engine level");

    assert_eq!(payment.status, PaymentStatus::Failed);
    assert!(payment.error.is_some());
    assert!(payment.tx_hash.is_none());
}

#[tokio::test]
async fn rejects_zero_amount() {
    let engine = engine_with_mock(MockAdapter::new("mock"));
    let result = engine
        .initiate(ALICE.into(), BOB.into(), 0, "XLM".into(), Urgency::Standard)
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn rejects_invalid_sender_address() {
    let engine = engine_with_mock(MockAdapter::new("mock"));
    let result = engine
        .initiate(
            "not-a-stellar-address".into(),
            BOB.into(),
            100,
            "XLM".into(),
            Urgency::Standard,
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn payment_is_retrievable_after_creation() {
    let engine = engine_with_mock(MockAdapter::new("mock"));

    let payment = engine
        .initiate(
            ALICE.into(),
            BOB.into(),
            250,
            "USDC".into(),
            Urgency::Urgent,
        )
        .await
        .unwrap();

    let fetched = engine.get(&payment.id).unwrap();
    assert_eq!(fetched.id, payment.id);
    assert_eq!(fetched.token, "USDC");
}

#[tokio::test]
async fn list_returns_all_payments() {
    let engine = engine_with_mock(MockAdapter::new("mock"));

    for amount in [100, 200, 300] {
        engine
            .initiate(
                ALICE.into(),
                BOB.into(),
                amount,
                "XLM".into(),
                Urgency::Standard,
            )
            .await
            .unwrap();
    }

    assert_eq!(engine.list().len(), 3);
}

#[tokio::test]
async fn simulation_returns_fee_estimate() {
    let engine = engine_with_mock(MockAdapter::new("mock"));

    let result = engine
        .simulate(ALICE.into(), BOB.into(), 1_000, "XLM".into(), Urgency::Fast)
        .await
        .unwrap();

    assert_eq!(result.chain, "mock");
    assert!(result.fee_stroops > 0);
    assert!(result.estimated_confirmation_seconds > 0);
}

#[tokio::test]
async fn idempotency_returns_cached_response() {
    let engine = engine_with_mock(MockAdapter::new("mock"));
    let body = serde_json::json!({"id": "cached-payment-id"});

    engine
        .idempotency()
        .set("key-001".to_string(), body.clone())
        .await
        .unwrap();
    let result = engine.idempotency().get("key-001").await.unwrap();
    assert_eq!(result.unwrap(), body);
}

#[tokio::test]
async fn health_reports_ok_with_working_adapter() {
    let engine = engine_with_mock(MockAdapter::new("mock"));
    let health = engine.health().await;
    assert_eq!(health.status, "ok");
}
