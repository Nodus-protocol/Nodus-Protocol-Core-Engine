#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::config::Network;
use crate::pool::soroban::{
    GetTransactionResponse, HttpTransport, LedgerEntry, RpcTransport, SendTransactionResponse,
    SimulateTransactionResponse,
};
use crate::pool::xdr::{self, ScMap};
use crate::utils::EngineError;

/// Maximum age (in seconds) for a provider's latest ledger before it is considered stale.
/// Matches the 15s threshold from operations/slo-alerts.yml StaleLedger alert.
pub const MAX_LEDGER_AGE_SECS: u64 = 15;

/// Maximum allowable ledger divergence across approved providers before read quorum fails closed.
pub const DEFAULT_MAX_DIVERGENCE_LEDGERS: u32 = 2;

/// Maximum ledgers a transaction simulation remains valid before expiring.
pub const MAX_SIMULATION_LEDGER_DRIFT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHealthSummary {
    pub endpoint: String,
    pub status: ProviderHealthStatus,
    pub latest_ledger: u32,
    pub latest_ledger_close_time: u64,
    pub network_passphrase: Option<String>,
    pub consecutive_failures: u32,
}

pub struct VerifiedRpcProvider {
    endpoint: String,
    transport: Box<dyn RpcTransport>,
    latest_ledger: AtomicU32,
    latest_ledger_close_time: AtomicU64,
    network_passphrase: RwLock<Option<String>>,
    protocol_version: AtomicU32,
    consecutive_failures: AtomicU32,
    circuit_open_until: RwLock<Option<Instant>>,
    failure_threshold: u32,
    reset_timeout: Duration,
}

impl VerifiedRpcProvider {
    pub fn new(endpoint: &str) -> Self {
        Self::with_transport(endpoint, Box::new(HttpTransport::new(endpoint)))
    }

    pub fn with_transport(endpoint: &str, transport: Box<dyn RpcTransport>) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            transport,
            latest_ledger: AtomicU32::new(0),
            latest_ledger_close_time: AtomicU64::new(0),
            network_passphrase: RwLock::new(None),
            protocol_version: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            circuit_open_until: RwLock::new(None),
            failure_threshold: 3,
            reset_timeout: Duration::from_secs(10),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn latest_ledger(&self) -> u32 {
        self.latest_ledger.load(Ordering::Relaxed)
    }

    pub fn latest_ledger_close_time(&self) -> u64 {
        self.latest_ledger_close_time.load(Ordering::Relaxed)
    }

    pub async fn is_circuit_open(&self) -> bool {
        let guard = self.circuit_open_until.read().await;
        if let Some(until) = *guard {
            Instant::now() < until
        } else {
            false
        }
    }

    pub async fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let mut guard = self.circuit_open_until.write().await;
        *guard = None;
    }

    pub async fn record_failure(&self) {
        let fails = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if fails >= self.failure_threshold {
            let mut guard = self.circuit_open_until.write().await;
            *guard = Some(Instant::now() + self.reset_timeout);
            tracing::warn!(
                endpoint = %self.endpoint,
                failures = fails,
                "provider circuit breaker opened"
            );
        }
    }

    pub async fn check_health(
        &self,
        expected_network: Network,
    ) -> Result<ProviderHealthSummary, EngineError> {
        if self.is_circuit_open().await {
            return Ok(ProviderHealthSummary {
                endpoint: self.endpoint.clone(),
                status: ProviderHealthStatus::Unhealthy,
                latest_ledger: self.latest_ledger(),
                latest_ledger_close_time: self.latest_ledger_close_time(),
                network_passphrase: self.network_passphrase.read().await.clone(),
                consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
            });
        }

        // 1. Verify getNetwork
        let network_val = match self.transport.call("getNetwork", json!({})).await {
            Ok(v) => v,
            Err(e) => {
                self.record_failure().await;
                return Err(e);
            }
        };

        let passphrase = network_val["passphrase"].as_str().unwrap_or_default();
        let protocol_version = network_val["protocolVersion"].as_u64().unwrap_or_default() as u32;

        let expected_passphrase = expected_network.passphrase();
        if passphrase != expected_passphrase {
            self.record_failure().await;
            return Err(EngineError::AdapterError(format!(
                "provider {} passphrase mismatch: got '{}', expected '{}'",
                self.endpoint, passphrase, expected_passphrase
            )));
        }

        {
            let mut guard = self.network_passphrase.write().await;
            *guard = Some(passphrase.to_string());
        }
        self.protocol_version
            .store(protocol_version, Ordering::Relaxed);

        // 2. Query getLatestLedger
        let ledger_val = match self.transport.call("getLatestLedger", json!({})).await {
            Ok(v) => v,
            Err(e) => {
                self.record_failure().await;
                return Err(e);
            }
        };

        let seq = ledger_val["sequence"]
            .as_u64()
            .map(|n| n as u32)
            .unwrap_or(0);
        let close_time = ledger_val["closeTime"].as_u64().unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });

        self.latest_ledger.store(seq, Ordering::Relaxed);
        self.latest_ledger_close_time
            .store(close_time, Ordering::Relaxed);
        self.record_success().await;

        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age = now_unix.saturating_sub(close_time);
        let status = if age > MAX_LEDGER_AGE_SECS {
            ProviderHealthStatus::Degraded
        } else {
            ProviderHealthStatus::Healthy
        };

        Ok(ProviderHealthSummary {
            endpoint: self.endpoint.clone(),
            status,
            latest_ledger: seq,
            latest_ledger_close_time: close_time,
            network_passphrase: Some(passphrase.to_string()),
            consecutive_failures: 0,
        })
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        if self.is_circuit_open().await {
            return Err(EngineError::AdapterError(format!(
                "provider {} circuit is open — request blocked",
                self.endpoint
            )));
        }

        match self.transport.call(method, params).await {
            Ok(val) => {
                self.record_success().await;
                Ok(val)
            }
            Err(e) => {
                self.record_failure().await;
                Err(e)
            }
        }
    }
}

pub struct ProviderPool {
    providers: Vec<Arc<VerifiedRpcProvider>>,
    expected_network: Network,
    divergence_threshold: u32,
    max_ledger_age_secs: u64,
}

impl ProviderPool {
    pub fn new(endpoints: &[String], expected_network: Network, divergence_threshold: u32) -> Self {
        let providers = endpoints
            .iter()
            .map(|ep| Arc::new(VerifiedRpcProvider::new(ep)))
            .collect();

        Self {
            providers,
            expected_network,
            divergence_threshold,
            max_ledger_age_secs: MAX_LEDGER_AGE_SECS,
        }
    }

    pub fn with_providers(
        providers: Vec<Arc<VerifiedRpcProvider>>,
        expected_network: Network,
        divergence_threshold: u32,
    ) -> Self {
        Self {
            providers,
            expected_network,
            divergence_threshold,
            max_ledger_age_secs: MAX_LEDGER_AGE_SECS,
        }
    }

    pub fn providers(&self) -> &[Arc<VerifiedRpcProvider>] {
        &self.providers
    }

    pub fn primary_endpoint(&self) -> String {
        self.providers
            .first()
            .map(|p| p.endpoint().to_string())
            .unwrap_or_default()
    }

    /// Returns all providers that are not currently blocked by an open circuit.
    pub async fn get_available_providers(&self) -> Vec<Arc<VerifiedRpcProvider>> {
        let mut available = Vec::new();
        for p in &self.providers {
            if !p.is_circuit_open().await {
                available.push(p.clone());
            }
        }
        available
    }

    /// Refresh health status across all providers and calculate divergence.
    pub async fn check_all_providers(&self) -> Vec<ProviderHealthSummary> {
        let mut summaries = Vec::new();
        for p in &self.providers {
            if let Ok(summary) = p.check_health(self.expected_network).await {
                summaries.push(summary);
            }
        }
        self.record_divergence_metrics();
        summaries
    }

    /// Calculates divergence between the highest and lowest ledger among healthy providers.
    pub fn calculate_divergence(&self) -> u32 {
        let ledgers: Vec<u32> = self
            .providers
            .iter()
            .map(|p| p.latest_ledger())
            .filter(|&l| l > 0)
            .collect();

        if ledgers.len() <= 1 {
            return 0;
        }

        let max = *ledgers.iter().max().unwrap_or(&0);
        let min = *ledgers.iter().min().unwrap_or(&0);
        max.saturating_sub(min)
    }

    pub fn record_divergence_metrics(&self) {
        let divergence = self.calculate_divergence();
        crate::observability::gauge("nodus_provider_divergence_ledgers", divergence as f64);
    }

    /// Executes read quorum / consistency policy to fetch pool instance storage.
    ///
    /// 1. Checks that at least one provider is healthy and not stale.
    /// 2. Asserts ledger divergence is within `divergence_threshold`.
    /// 3. Reads and decodes the instance storage map from the highest confirmed ledger.
    pub async fn read_reserves_with_quorum(
        &self,
        contract_id: &str,
    ) -> Result<(ScMap, u32, u64), EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Err(EngineError::AdapterError(
                "all RPC providers unavailable (circuits open or down)".into(),
            ));
        }

        // Refresh health / latest ledgers if needed
        for p in &available {
            let _ = p.check_health(self.expected_network).await;
        }

        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Filter out stale providers (> MAX_LEDGER_AGE_SECS)
        let non_stale: Vec<Arc<VerifiedRpcProvider>> = available
            .into_iter()
            .filter(|p| {
                let close = p.latest_ledger_close_time();
                close == 0 || (now_unix.saturating_sub(close) <= self.max_ledger_age_secs)
            })
            .collect();

        if non_stale.is_empty() {
            crate::observability::gauge(
                "nodus_quote_age_seconds",
                self.max_ledger_age_secs as f64 + 1.0,
            );
            return Err(EngineError::Conflict(
                "all RPC providers have stale ledgers — refusing to serve unsafe quote".into(),
            ));
        }

        let divergence = self.calculate_divergence();
        crate::observability::gauge("nodus_provider_divergence_ledgers", divergence as f64);

        if self.providers.len() > 1 && divergence > self.divergence_threshold {
            return Err(EngineError::Conflict(format!(
                "RPC providers divergent by {} ledgers (limit: {}) — state read blocked",
                divergence, self.divergence_threshold
            )));
        }

        // Sort by latest ledger descending to read from the most up-to-date provider
        let mut sorted = non_stale;
        sorted.sort_by_key(|p| std::cmp::Reverse(p.latest_ledger()));

        let key = xdr::contract_instance_ledger_key(contract_id)?;

        let mut last_err = None;
        for provider in sorted {
            match provider
                .call("getLedgerEntries", json!({ "keys": [key] }))
                .await
            {
                Ok(result) => {
                    let entries = result["entries"].as_array().cloned().unwrap_or_default();

                    if let Some(first) = entries.first() {
                        let entry: Result<LedgerEntry, _> = serde_json::from_value(first.clone());
                        if let Ok(entry) = entry {
                            let map = xdr::decode_instance_storage(&entry.xdr)?;
                            let source_ledger = entry
                                .last_modified
                                .unwrap_or_else(|| provider.latest_ledger());
                            let close_time = provider.latest_ledger_close_time();
                            return Ok((map, source_ledger, close_time));
                        }
                    } else {
                        return Err(EngineError::NotFound("contract instance not found".into()));
                    }
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            EngineError::AdapterError(
                "failed to read ledger entries from approved providers".into(),
            )
        }))
    }

    /// Queries `getLedgerEntries` across available providers with failover.
    pub async fn get_ledger_entries(
        &self,
        keys: Vec<String>,
    ) -> Result<Vec<LedgerEntry>, EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Err(EngineError::AdapterError(
                "all RPC providers unavailable".into(),
            ));
        }

        let mut last_err = None;
        for provider in available {
            match provider
                .call("getLedgerEntries", json!({ "keys": keys }))
                .await
            {
                Ok(result) => {
                    let entries = result["entries"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|e| serde_json::from_value(e).ok())
                        .collect();
                    return Ok(entries);
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            EngineError::AdapterError("getLedgerEntries failed on all providers".into())
        }))
    }

    /// Simulates a transaction, recording the `source_ledger` and `provider_id`.
    pub async fn simulate_transaction(
        &self,
        xdr: &str,
    ) -> Result<(SimulateTransactionResponse, u32, String), EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Err(EngineError::AdapterError(
                "all RPC providers unavailable".into(),
            ));
        }

        let mut last_err = None;
        for provider in available {
            match provider
                .call("simulateTransaction", json!({ "transaction": xdr }))
                .await
            {
                Ok(val) => {
                    let resp: SimulateTransactionResponse =
                        serde_json::from_value(val).map_err(|e| {
                            EngineError::Internal(format!("parse simulate result: {e}"))
                        })?;
                    let source_ledger = resp
                        .latest_ledger
                        .unwrap_or_else(|| provider.latest_ledger());
                    return Ok((resp, source_ledger, provider.endpoint().to_string()));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            EngineError::AdapterError("simulateTransaction failed on all providers".into())
        }))
    }

    /// Submits a transaction with failover to secondary providers on transport errors.
    pub async fn send_transaction(
        &self,
        signed_xdr: &str,
    ) -> Result<SendTransactionResponse, EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Err(EngineError::AdapterError(
                "all RPC providers unavailable".into(),
            ));
        }

        let mut last_err = None;
        for provider in available {
            match provider
                .call("sendTransaction", json!({ "transaction": signed_xdr }))
                .await
            {
                Ok(val) => {
                    let resp: SendTransactionResponse =
                        serde_json::from_value(val).map_err(|e| {
                            EngineError::Internal(format!("parse sendTransaction result: {e}"))
                        })?;
                    return Ok(resp);
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %provider.endpoint(),
                        error = %e,
                        "sendTransaction failed on provider, trying fallback"
                    );
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            EngineError::AdapterError("sendTransaction failed across all providers".into())
        }))
    }

    /// Polls `getTransaction` across approved providers, returning as soon as a non-NOT_FOUND response is found.
    pub async fn get_transaction(&self, hash: &str) -> Result<GetTransactionResponse, EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Err(EngineError::AdapterError(
                "all RPC providers unavailable".into(),
            ));
        }

        let mut last_response = None;
        for provider in &available {
            if let Ok(val) = provider
                .call("getTransaction", json!({ "hash": hash }))
                .await
            {
                if let Ok(resp) = serde_json::from_value::<GetTransactionResponse>(val) {
                    if resp.status != "NOT_FOUND" {
                        return Ok(resp);
                    }
                    last_response = Some(resp);
                }
            }
        }

        if let Some(resp) = last_response {
            Ok(resp)
        } else {
            Ok(GetTransactionResponse {
                status: "NOT_FOUND".to_string(),
                latest_ledger: None,
                result_xdr: None,
                envelope_xdr: None,
            })
        }
    }

    /// Verifies that all approved providers unanimously return `NOT_FOUND` AND that each provider's
    /// latest ledger sequence has reached or exceeded `min_ledger_threshold`.
    pub async fn check_all_approved_unanimous_not_found(
        &self,
        hash: &str,
        min_ledger_threshold: u32,
    ) -> Result<bool, EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Ok(false);
        }

        for provider in &available {
            let _ = provider.check_health(self.expected_network).await;
            if provider.latest_ledger() < min_ledger_threshold {
                // Provider has not yet reached the required ledger height
                return Ok(false);
            }

            match provider
                .call("getTransaction", json!({ "hash": hash }))
                .await
            {
                Ok(val) => {
                    if let Ok(resp) = serde_json::from_value::<GetTransactionResponse>(val) {
                        if resp.status != "NOT_FOUND" {
                            return Ok(false);
                        }
                    }
                }
                Err(_) => {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    pub fn latest_consensus_ledger(&self) -> u32 {
        self.providers
            .iter()
            .map(|p| p.latest_ledger())
            .max()
            .unwrap_or(0)
    }

    pub fn latest_consensus_close_time(&self) -> u64 {
        self.providers
            .iter()
            .map(|p| p.latest_ledger_close_time())
            .max()
            .unwrap_or(0)
    }
}

#[async_trait]
impl RpcTransport for ProviderPool {
    async fn call(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        let available = self.get_available_providers().await;
        if available.is_empty() {
            return Err(EngineError::AdapterError(
                "all RPC providers unavailable".into(),
            ));
        }

        let mut last_err = None;
        for provider in available {
            match provider.call(method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            EngineError::AdapterError(format!("method {method} failed across all providers"))
        }))
    }
}
