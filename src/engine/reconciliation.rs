#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::config::Network;
use crate::idempotency::ClaimStore;
use crate::pool::provider::ProviderPool;
use crate::utils::EngineError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    RpcQuorum,
    EventIndex,
    Hybrid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    PendingInclusion,
    ProviderDisagreement,
    IndexLagging,
    RpcRetentionExhaustedAwaitingIndex,
    TransientOutage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReconciliationOutcome {
    Success {
        ledger: u32,
        result_xdr: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        executed_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    Failed {
        ledger: u32,
        error_result_xdr: String,
    },
    Expired {
        observed_ledger: u32,
        max_time_or_ledger_exceeded: u64,
        source_seq_consumed: bool,
    },
    StillUnknown {
        reason: UnknownReason,
        consensus_ledger: Option<u32>,
        checked_providers: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationResult {
    pub tx_hash: String,
    pub outcome: ReconciliationOutcome,
    pub evidence_source: EvidenceSource,
    pub verified_at: chrono::DateTime<chrono::Utc>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub tx_hash: String,
    pub contract_id: String,
    pub ledger: u32,
    pub successful: bool,
    pub result_xdr: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventIndexSyncStatus {
    pub caught_up: bool,
    pub last_indexed_ledger: u32,
    pub last_indexed_close_time: u64,
    pub lag_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationStatusResponse {
    pub caught_up: bool,
    pub lag: u64,
    pub lag_seconds: u64,
    pub last_indexed_ledger: u32,
}

#[async_trait]
pub trait EventIndexClient: Send + Sync {
    async fn find_transaction_event(
        &self,
        tx_hash: &str,
        contract_id: &str,
    ) -> Result<Option<EventRecord>, EngineError>;

    async fn get_sync_status(&self) -> Result<EventIndexSyncStatus, EngineError>;
}

pub struct HttpEventIndexClient {
    endpoint: String,
    client: reqwest::Client,
}

impl HttpEventIndexClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("event index http client"),
        }
    }
}

#[async_trait]
impl EventIndexClient for HttpEventIndexClient {
    async fn find_transaction_event(
        &self,
        tx_hash: &str,
        contract_id: &str,
    ) -> Result<Option<EventRecord>, EngineError> {
        let url = format!(
            "{}/api/v1/events?hash={}&contract={}",
            self.endpoint, tx_hash, contract_id
        );
        let resp = crate::observability::propagate(self.client.get(&url))
            .send()
            .await
            .map_err(|e| EngineError::NetworkError(format!("event index request: {e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(EngineError::AdapterError(format!(
                "event index returned {}",
                resp.status()
            )));
        }

        let events: Vec<EventRecord> = resp
            .json()
            .await
            .map_err(|e| EngineError::Internal(format!("parse event record: {e}")))?;
        Ok(events.into_iter().next())
    }

    async fn get_sync_status(&self) -> Result<EventIndexSyncStatus, EngineError> {
        let url = format!("{}/internal/reconciliation/status", self.endpoint);
        let resp = crate::observability::propagate(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                EngineError::NetworkError(format!("event index sync status request: {e}"))
            })?;

        if !resp.status().is_success() {
            return Err(EngineError::AdapterError(format!(
                "event index status returned {}",
                resp.status()
            )));
        }

        resp.json()
            .await
            .map_err(|e| EngineError::Internal(format!("parse event index status: {e}")))
    }
}

pub struct MockEventIndexClient {
    events: DashMap<String, EventRecord>,
    last_indexed_ledger: AtomicU64,
    lag_seconds: AtomicU64,
}

impl MockEventIndexClient {
    pub fn new() -> Self {
        Self {
            events: DashMap::new(),
            last_indexed_ledger: AtomicU64::new(0),
            lag_seconds: AtomicU64::new(0),
        }
    }

    pub fn insert_event(&self, event: EventRecord) {
        self.events.insert(event.tx_hash.clone(), event);
    }

    pub fn set_last_indexed_ledger(&self, ledger: u32) {
        self.last_indexed_ledger
            .store(ledger as u64, Ordering::Relaxed);
    }

    pub fn set_lag_seconds(&self, lag: u64) {
        self.lag_seconds.store(lag, Ordering::Relaxed);
    }
}

impl Default for MockEventIndexClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventIndexClient for MockEventIndexClient {
    async fn find_transaction_event(
        &self,
        tx_hash: &str,
        _contract_id: &str,
    ) -> Result<Option<EventRecord>, EngineError> {
        Ok(self.events.get(tx_hash).map(|e| e.clone()))
    }

    async fn get_sync_status(&self) -> Result<EventIndexSyncStatus, EngineError> {
        let last_ledger = self.last_indexed_ledger.load(Ordering::Relaxed) as u32;
        let lag = self.lag_seconds.load(Ordering::Relaxed);
        Ok(EventIndexSyncStatus {
            caught_up: lag == 0,
            last_indexed_ledger: last_ledger,
            last_indexed_close_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            lag_seconds: lag,
        })
    }
}

pub struct ReconciliationManager {
    provider_pool: Arc<ProviderPool>,
    event_index: Arc<dyn EventIndexClient>,
    #[allow(dead_code)]
    claim_store: Option<Arc<dyn ClaimStore>>,
    #[allow(dead_code)]
    network: Network,
    reconciliation_lag_seconds: Arc<AtomicU64>,
}

impl ReconciliationManager {
    pub fn new(
        provider_pool: Arc<ProviderPool>,
        event_index: Option<Arc<dyn EventIndexClient>>,
        claim_store: Option<Arc<dyn ClaimStore>>,
        network: Network,
    ) -> Self {
        let event_index = event_index.unwrap_or_else(|| Arc::new(MockEventIndexClient::new()));
        Self {
            provider_pool,
            event_index,
            claim_store,
            network,
            reconciliation_lag_seconds: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn reconciliation_lag_seconds(&self) -> u64 {
        self.reconciliation_lag_seconds.load(Ordering::Relaxed)
    }

    /// Evaluates bounded evidence from Soroban RPC providers and the Backend's
    /// durable event index to strictly classify a transaction hash.
    pub async fn classify(
        &self,
        tx_hash: &str,
        contract_id: &str,
        max_ledger: Option<u32>,
        max_time: Option<u64>,
    ) -> Result<ReconciliationResult, EngineError> {
        let verified_at = chrono::Utc::now();
        let checked_providers: Vec<String> = self
            .provider_pool
            .providers()
            .iter()
            .map(|p| p.endpoint().to_string())
            .collect();

        // 1. Query approved RPC providers
        let rpc_res = self.provider_pool.get_transaction(tx_hash).await;
        match rpc_res {
            Ok(resp) if resp.status == "SUCCESS" => {
                let ledger = resp.latest_ledger.unwrap_or(0);
                Ok(ReconciliationResult {
                    tx_hash: tx_hash.to_string(),
                    outcome: ReconciliationOutcome::Success {
                        ledger,
                        result_xdr: resp.result_xdr.unwrap_or_default(),
                        executed_at: Some(verified_at),
                    },
                    evidence_source: EvidenceSource::RpcQuorum,
                    verified_at,
                    attempts: 1,
                })
            }
            Ok(resp) if resp.status == "FAILED" => {
                let ledger = resp.latest_ledger.unwrap_or(0);
                Ok(ReconciliationResult {
                    tx_hash: tx_hash.to_string(),
                    outcome: ReconciliationOutcome::Failed {
                        ledger,
                        error_result_xdr: resp.result_xdr.unwrap_or_default(),
                    },
                    evidence_source: EvidenceSource::RpcQuorum,
                    verified_at,
                    attempts: 1,
                })
            }
            Ok(resp) if resp.status == "NOT_FOUND" => {
                let consensus_ledger = self.provider_pool.latest_consensus_ledger();
                let _consensus_close_time = self.provider_pool.latest_consensus_close_time();
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                // 1. Check durable event index first. If the transaction was mined,
                // the index holds authoritative success/failure event records.
                if let Ok(Some(ev)) = self
                    .event_index
                    .find_transaction_event(tx_hash, contract_id)
                    .await
                {
                    if ev.successful {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::Success {
                                ledger: ev.ledger,
                                result_xdr: ev.result_xdr.unwrap_or_default(),
                                executed_at: Some(verified_at),
                            },
                            evidence_source: EvidenceSource::EventIndex,
                            verified_at,
                            attempts: 1,
                        });
                    } else {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::Failed {
                                ledger: ev.ledger,
                                error_result_xdr: ev.result_xdr.unwrap_or_default(),
                            },
                            evidence_source: EvidenceSource::EventIndex,
                            verified_at,
                            attempts: 1,
                        });
                    }
                }

                // 2. Check event index synchronization status and ledger progression
                if let Ok(sync) = self.event_index.get_sync_status().await {
                    self.reconciliation_lag_seconds
                        .store(sync.lag_seconds, Ordering::Relaxed);
                    crate::observability::gauge(
                        "nodus_reconciliation_lag_seconds",
                        sync.lag_seconds as f64,
                    );

                    let index_past_ledger =
                        max_ledger.is_some_and(|ml| sync.last_indexed_ledger > ml);
                    let index_past_time =
                        max_time.is_some_and(|mt| sync.last_indexed_close_time > mt);
                    let consensus_past_ledger = max_ledger.is_some_and(|ml| consensus_ledger > ml);
                    let time_past = max_time.is_some_and(|mt| now_unix > mt);

                    if index_past_ledger
                        || index_past_time
                        || (sync.caught_up && (consensus_past_ledger || time_past))
                    {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::Expired {
                                observed_ledger: consensus_ledger.max(sync.last_indexed_ledger),
                                max_time_or_ledger_exceeded: max_ledger
                                    .map(|l| l as u64)
                                    .or(max_time)
                                    .unwrap_or(0),
                                source_seq_consumed: false,
                            },
                            evidence_source: EvidenceSource::Hybrid,
                            verified_at,
                            attempts: 1,
                        });
                    }

                    if sync.lag_seconds > 0 {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::StillUnknown {
                                reason: UnknownReason::IndexLagging,
                                consensus_ledger: Some(consensus_ledger),
                                checked_providers,
                            },
                            evidence_source: EvidenceSource::Hybrid,
                            verified_at,
                            attempts: 1,
                        });
                    }
                }

                // 3. If bounded max_time or max_ledger hasn't arrived, or index hasn't caught up,
                // the transaction might still be in the mempool or pending block inclusion.
                Ok(ReconciliationResult {
                    tx_hash: tx_hash.to_string(),
                    outcome: ReconciliationOutcome::StillUnknown {
                        reason: UnknownReason::PendingInclusion,
                        consensus_ledger: Some(consensus_ledger),
                        checked_providers,
                    },
                    evidence_source: EvidenceSource::RpcQuorum,
                    verified_at,
                    attempts: 1,
                })
            }
            Err(_) => {
                // RPC returned an error (outage, rate limit, or retention exhausted).
                // Fall back to Durable Event Index as source of truth.
                if let Ok(Some(ev)) = self
                    .event_index
                    .find_transaction_event(tx_hash, contract_id)
                    .await
                {
                    if ev.successful {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::Success {
                                ledger: ev.ledger,
                                result_xdr: ev.result_xdr.unwrap_or_default(),
                                executed_at: Some(verified_at),
                            },
                            evidence_source: EvidenceSource::EventIndex,
                            verified_at,
                            attempts: 1,
                        });
                    } else {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::Failed {
                                ledger: ev.ledger,
                                error_result_xdr: ev.result_xdr.unwrap_or_default(),
                            },
                            evidence_source: EvidenceSource::EventIndex,
                            verified_at,
                            attempts: 1,
                        });
                    }
                }

                // If event index does not have it, verify sync status before declaring unknown
                if let Ok(sync) = self.event_index.get_sync_status().await {
                    self.reconciliation_lag_seconds
                        .store(sync.lag_seconds, Ordering::Relaxed);
                    crate::observability::gauge(
                        "nodus_reconciliation_lag_seconds",
                        sync.lag_seconds as f64,
                    );

                    let index_past_ledger =
                        max_ledger.is_some_and(|ml| sync.last_indexed_ledger > ml);
                    let index_past_time =
                        max_time.is_some_and(|mt| sync.last_indexed_close_time > mt);

                    if index_past_ledger || index_past_time {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::Expired {
                                observed_ledger: sync.last_indexed_ledger,
                                max_time_or_ledger_exceeded: max_ledger
                                    .map(|l| l as u64)
                                    .or(max_time)
                                    .unwrap_or(0),
                                source_seq_consumed: false,
                            },
                            evidence_source: EvidenceSource::EventIndex,
                            verified_at,
                            attempts: 1,
                        });
                    }

                    if sync.lag_seconds > 0 {
                        return Ok(ReconciliationResult {
                            tx_hash: tx_hash.to_string(),
                            outcome: ReconciliationOutcome::StillUnknown {
                                reason: UnknownReason::IndexLagging,
                                consensus_ledger: None,
                                checked_providers,
                            },
                            evidence_source: EvidenceSource::EventIndex,
                            verified_at,
                            attempts: 1,
                        });
                    }
                }

                Ok(ReconciliationResult {
                    tx_hash: tx_hash.to_string(),
                    outcome: ReconciliationOutcome::StillUnknown {
                        reason: UnknownReason::RpcRetentionExhaustedAwaitingIndex,
                        consensus_ledger: None,
                        checked_providers,
                    },
                    evidence_source: EvidenceSource::EventIndex,
                    verified_at,
                    attempts: 1,
                })
            }
            _ => Ok(ReconciliationResult {
                tx_hash: tx_hash.to_string(),
                outcome: ReconciliationOutcome::StillUnknown {
                    reason: UnknownReason::TransientOutage,
                    consensus_ledger: None,
                    checked_providers,
                },
                evidence_source: EvidenceSource::RpcQuorum,
                verified_at,
                attempts: 1,
            }),
        }
    }

    /// Exposes reconciliation status for /internal/reconciliation/status endpoint.
    pub async fn status(&self) -> Result<ReconciliationStatusResponse, EngineError> {
        let sync = self.event_index.get_sync_status().await?;
        self.reconciliation_lag_seconds
            .store(sync.lag_seconds, Ordering::Relaxed);
        crate::observability::gauge("nodus_reconciliation_lag_seconds", sync.lag_seconds as f64);
        Ok(ReconciliationStatusResponse {
            caught_up: sync.caught_up,
            lag: sync.lag_seconds,
            lag_seconds: sync.lag_seconds,
            last_indexed_ledger: sync.last_indexed_ledger,
        })
    }
}
