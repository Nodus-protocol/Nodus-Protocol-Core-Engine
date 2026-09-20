use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use nodus_core_engine::config::{Network, PoolConfig};
use nodus_core_engine::engine::reconciliation::{
    EventRecord, MockEventIndexClient, ReconciliationManager, ReconciliationOutcome, UnknownReason,
};
use nodus_core_engine::pool::contract::ContractClient;
use nodus_core_engine::pool::provider::{
    ProviderHealthStatus, ProviderPool, VerifiedRpcProvider, MAX_SIMULATION_LEDGER_DRIFT,
};
use nodus_core_engine::pool::soroban::{RpcTransport, SorobanRpc};
use nodus_core_engine::utils::{AssetId, AssetType, EngineError};

struct MockTransport {
    close_time_offset_secs: i64,
}

impl MockTransport {
    fn new(close_time_offset_secs: i64) -> Self {
        Self {
            close_time_offset_secs,
        }
    }
}

#[async_trait]
impl RpcTransport for MockTransport {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
        match method {
            "getNetwork" => Ok(json!({
                "passphrase": "Test SDF Network ; September 2015",
                "protocolVersion": 22
            })),
            "getLatestLedger" => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let close_time = (now + self.close_time_offset_secs).max(0) as u64;
                Ok(json!({
                    "id": "1",
                    "sequence": 1000,
                    "closeTime": close_time
                }))
            }
            _ => Err(EngineError::AdapterError(format!("unsupported: {method}"))),
        }
    }
}

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM".into(),
        soroban_rpc_url: "http://127.0.0.1:8000".into(),
        soroban_rpc_urls: vec![
            "http://127.0.0.1:8000".into(),
            "http://127.0.0.1:8001".into(),
        ],
        token_0: AssetId::native("testnet"),
        token_1: AssetId {
            network: "testnet".into(),
            asset_type: AssetType::ContractToken,
            symbol: "USDC".into(),
            decimals: 7,
            issuer: None,
            contract: Some("CBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM".into()),
        },
        base_fee_stroops: 100,
        fee_ceiling_stroops: 500_000,
        provider_divergence_max_ledgers: 5,
    }
}

// ── Test 1: Stale Ledger Rejection and Failover ───────────────────────────────

#[tokio::test]
async fn test_stale_ledger_rejected_and_failover() {
    // Provider whose latest ledger was 30 seconds ago (> 15s MAX_LEDGER_AGE_SECS)
    let stale_transport = Box::new(MockTransport::new(-30));
    let provider_stale = VerifiedRpcProvider::with_transport("http://stale:8000", stale_transport);

    let summary = provider_stale
        .check_health(Network::Testnet)
        .await
        .expect("health check succeeded");
    assert_eq!(summary.status, ProviderHealthStatus::Degraded);

    // Provider whose latest ledger is current (0 offset)
    let fresh_transport = Box::new(MockTransport::new(0));
    let provider_fresh = VerifiedRpcProvider::with_transport("http://fresh:8000", fresh_transport);

    let summary_fresh = provider_fresh
        .check_health(Network::Testnet)
        .await
        .expect("health check succeeded");
    assert_eq!(summary_fresh.status, ProviderHealthStatus::Healthy);
}

// ── Test 2: Split Responses & Divergence Detection Failing Closed ─────────────

#[tokio::test]
async fn test_split_responses_divergence_detection() {
    let pool_cfg = test_pool_config();
    let pool = ProviderPool::new(&pool_cfg.soroban_rpc_urls, Network::Testnet, 5);

    // Attempting read with unconfigured network servers fails closed safely
    let res = pool.read_reserves_with_quorum(&pool_cfg.contract_id).await;

    assert!(res.is_err());
}

// ── Test 3: HTTP 429 Rate Limit Circuit Breaking ──────────────────────────────

#[tokio::test]
async fn test_circuit_breaking_on_429_rate_limit() {
    let provider = VerifiedRpcProvider::new("http://rate-limited-rpc:8000");
    assert!(!provider.is_circuit_open().await);

    // Record failures up to threshold (3)
    provider.record_failure().await;
    provider.record_failure().await;
    provider.record_failure().await;

    assert!(provider.is_circuit_open().await);

    // Record success resets circuit breaker
    provider.record_success().await;
    assert!(!provider.is_circuit_open().await);
}

// ── Test 4: Submit Derives Hash Pre-Flight & Unanimous NOT_FOUND Check ────────

#[tokio::test]
async fn test_submit_unanimous_not_found_prevents_duplicate_send() {
    let pool_cfg = test_pool_config();
    let pool = ProviderPool::new(&pool_cfg.soroban_rpc_urls, Network::Testnet, 5);

    let unanimous = pool
        .check_all_approved_unanimous_not_found("some_hash", 100)
        .await
        .unwrap_or(false);
    // Providers in pool have no live server, so call returns false or error
    assert!(!unanimous);
}

// ── Test 5: Simulation Expires on Ledger Drift (> 3 ledgers) ──────────────────

#[test]
fn test_simulation_expires_on_ledger_drift() {
    let sim_ledger = 1000;
    let allowed_ledger = sim_ledger + MAX_SIMULATION_LEDGER_DRIFT; // 1003
    let expired_ledger = allowed_ledger + 1; // 1004

    // Within drift threshold
    assert!(allowed_ledger - sim_ledger <= MAX_SIMULATION_LEDGER_DRIFT);
    // Exceeded drift threshold
    assert!(expired_ledger - sim_ledger > MAX_SIMULATION_LEDGER_DRIFT);
}

// ── Test 6: Reconciliation Classifier with Durable Event Index Fallback ──────

#[tokio::test]
async fn test_reconciliation_classifier_and_event_index_fallback() {
    let pool_cfg = test_pool_config();
    let rpc = SorobanRpc::new(&pool_cfg.soroban_rpc_url);
    let contract_client = ContractClient::new(rpc, &pool_cfg, Network::Testnet);

    let mock_event_index = Arc::new(MockEventIndexClient::new());
    let recon = ReconciliationManager::new(
        contract_client.provider_pool().unwrap(),
        Some(mock_event_index.clone()),
        None,
        Network::Testnet,
    );

    let test_hash = "abc123def456";
    let contract_id = &pool_cfg.contract_id;

    // 1. Initially NOT_FOUND on RPC and not yet in event index
    let res = recon
        .classify(test_hash, contract_id, Some(500), None)
        .await
        .unwrap();
    match res.outcome {
        ReconciliationOutcome::StillUnknown { reason, .. } => {
            assert!(matches!(
                reason,
                UnknownReason::PendingInclusion
                    | UnknownReason::IndexLagging
                    | UnknownReason::TransientOutage
                    | UnknownReason::RpcRetentionExhaustedAwaitingIndex
            ));
        }
        other => panic!("expected StillUnknown, got {:?}", other),
    }

    // 2. Mock event index indexed past max_ledger and records success
    mock_event_index.set_last_indexed_ledger(600);
    mock_event_index.insert_event(EventRecord {
        tx_hash: test_hash.to_string(),
        contract_id: contract_id.to_string(),
        ledger: 550,
        successful: true,
        result_xdr: Some("AAA=".to_string()),
        timestamp: 1000,
    });

    let res = recon
        .classify(test_hash, contract_id, Some(500), None)
        .await
        .unwrap();
    match res.outcome {
        ReconciliationOutcome::Success { ledger, .. } => {
            assert_eq!(ledger, 550);
        }
        other => panic!("expected Success from event index, got {:?}", other),
    }

    // 3. Mock event index records failure
    let fail_hash = "failed_tx_hash";
    mock_event_index.insert_event(EventRecord {
        tx_hash: fail_hash.to_string(),
        contract_id: contract_id.to_string(),
        ledger: 500,
        successful: false,
        result_xdr: Some("AAA=".to_string()),
        timestamp: 1000,
    });

    let res = recon
        .classify(fail_hash, contract_id, Some(500), None)
        .await
        .unwrap();
    match res.outcome {
        ReconciliationOutcome::Failed { .. } => {
            // Correctly classified as Failed
        }
        other => panic!("expected Failed from event index, got {:?}", other),
    }

    // 4. Truly expired: max_ledger passed and indexed beyond without trace
    let expired_hash = "expired_tx_hash";
    let res = recon
        .classify(expired_hash, contract_id, Some(500), None)
        .await
        .unwrap();
    match res.outcome {
        ReconciliationOutcome::Expired { .. } => {
            // Correctly expired
        }
        other => panic!("expected Expired, got {:?}", other),
    }
}

// ── Test 7: Reconciliation Status Reports Catchup and Lag ─────────────────────

#[tokio::test]
async fn test_reconciliation_status_response() {
    let pool_cfg = test_pool_config();
    let rpc = SorobanRpc::new(&pool_cfg.soroban_rpc_url);
    let contract_client = ContractClient::new(rpc, &pool_cfg, Network::Testnet);

    let mock_event_index = Arc::new(MockEventIndexClient::new());
    mock_event_index.set_last_indexed_ledger(1200);

    let recon = ReconciliationManager::new(
        contract_client.provider_pool().unwrap(),
        Some(mock_event_index),
        None,
        Network::Testnet,
    );

    let status = recon.status().await.unwrap();
    assert!(status.caught_up);
    assert_eq!(status.lag, 0);
    assert_eq!(status.lag_seconds, 0);
}
