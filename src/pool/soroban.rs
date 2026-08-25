use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::utils::EngineError;

/// A JSON-RPC transport: given a Soroban RPC method name and its `params`,
/// returns the decoded `result` object (or an `Err` for a transport failure
/// or an RPC-level `error`). Split out from [`SorobanRpc`] so tests can
/// exercise the full prepare/simulate/validate pipeline against canned
/// responses instead of a live network — see `FakeTransport` in
/// `tests/soroban_prepare_test.rs`.
#[async_trait]
pub trait RpcTransport: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Value, EngineError>;
}

pub struct HttpTransport {
    endpoint: String,
    client: reqwest::Client,
    id: AtomicU64,
}

impl HttpTransport {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("soroban rpc client"),
            id: AtomicU64::new(1),
        }
    }
}

#[async_trait]
impl RpcTransport for HttpTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.id.fetch_add(1, Ordering::Relaxed),
            "method": method,
            "params": params,
        });

        let resp = crate::observability::propagate(self.client.post(&self.endpoint))
            .json(&body)
            .send()
            .await
            .map_err(|e| EngineError::NetworkError(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(EngineError::NetworkError(format!(
                "soroban rpc returned {}",
                resp.status()
            )));
        }

        let val: Value = resp
            .json()
            .await
            .map_err(|e| EngineError::Internal(format!("parse rpc response: {e}")))?;

        if let Some(err) = val.get("error") {
            return Err(EngineError::AdapterError(format!(
                "soroban rpc {method} error: {err}"
            )));
        }

        Ok(val.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[derive(Debug, Deserialize)]
pub struct LedgerEntry {
    #[allow(dead_code)]
    pub key: String,
    pub xdr: String,
    #[serde(rename = "lastModifiedLedgerSeq")]
    #[allow(dead_code)]
    pub last_modified: Option<u32>,
}

/// A single invocation's simulated result: the return-value XDR and the
/// authorization entries Soroban determined that invocation requires.
#[derive(Debug, Clone, Deserialize)]
pub struct SimulateResultItem {
    /// The invocation's return value, XDR-encoded. Not currently decoded by
    /// the prepare pipeline (only `auth` is consumed), kept for typed RPC
    /// response compatibility and future use (e.g. surfacing the return
    /// value in a review summary).
    #[allow(dead_code)]
    pub xdr: String,
    #[serde(default)]
    pub auth: Vec<String>,
}

/// Present on a `simulateTransaction` response when ledger entries the call
/// touches have expired and must be restored first. The prepare pipeline
/// fails closed on this (see `prepare::prepare`) rather than attempting a
/// restoration transaction itself, so these fields are read only for their
/// presence (`Option::is_some`), not their content — kept for typed RPC
/// response compatibility.
#[derive(Debug, Clone, Deserialize)]
pub struct RestorePreamble {
    #[serde(rename = "transactionData")]
    #[allow(dead_code)]
    pub transaction_data: String,
    #[serde(rename = "minResourceFee")]
    #[allow(dead_code)]
    pub min_resource_fee: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SimulateTransactionResponse {
    pub error: Option<String>,
    #[serde(rename = "latestLedger")]
    #[allow(dead_code)]
    pub latest_ledger: Option<u32>,
    #[serde(rename = "transactionData")]
    pub transaction_data: Option<String>,
    #[serde(rename = "minResourceFee")]
    pub min_resource_fee: Option<String>,
    #[serde(default)]
    pub results: Vec<SimulateResultItem>,
    #[serde(rename = "restorePreamble")]
    pub restore_preamble: Option<RestorePreamble>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendTransactionResponse {
    pub status: String,
    pub hash: String,
    #[serde(rename = "latestLedger")]
    #[allow(dead_code)]
    pub latest_ledger: Option<u32>,
    #[serde(rename = "errorResultXdr")]
    pub error_result_xdr: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetTransactionResponse {
    pub status: String,
    #[serde(rename = "latestLedger")]
    #[allow(dead_code)]
    pub latest_ledger: Option<u32>,
    #[serde(rename = "resultXdr")]
    pub result_xdr: Option<String>,
    #[serde(rename = "envelopeXdr")]
    #[allow(dead_code)]
    pub envelope_xdr: Option<String>,
}

pub struct SorobanRpc {
    transport: Box<dyn RpcTransport>,
}

impl SorobanRpc {
    pub fn new(endpoint: &str) -> Self {
        Self {
            transport: Box::new(HttpTransport::new(endpoint)),
        }
    }

    /// Test/embedding hook: build a client over any [`RpcTransport`],
    /// bypassing the real HTTP client entirely. Only called from
    /// `tests/soroban_prepare_test.rs` today — the `nodus-core-engine`
    /// binary itself always goes through `new` — hence the narrow allow.
    #[allow(dead_code)]
    pub fn with_transport(transport: Box<dyn RpcTransport>) -> Self {
        Self { transport }
    }

    pub async fn get_ledger_entries(
        &self,
        keys: Vec<String>,
    ) -> Result<Vec<LedgerEntry>, EngineError> {
        let result = self
            .transport
            .call("getLedgerEntries", json!({ "keys": keys }))
            .await?;

        let entries = result["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| serde_json::from_value(e).ok())
            .collect();

        Ok(entries)
    }

    pub async fn simulate_transaction(
        &self,
        xdr: &str,
    ) -> Result<SimulateTransactionResponse, EngineError> {
        let result = self
            .transport
            .call("simulateTransaction", json!({ "transaction": xdr }))
            .await?;

        serde_json::from_value(result)
            .map_err(|e| EngineError::Internal(format!("parse simulate result: {e}")))
    }

    pub async fn send_transaction(
        &self,
        xdr: &str,
    ) -> Result<SendTransactionResponse, EngineError> {
        let result = self
            .transport
            .call("sendTransaction", json!({ "transaction": xdr }))
            .await?;

        serde_json::from_value(result)
            .map_err(|e| EngineError::Internal(format!("parse sendTransaction result: {e}")))
    }

    pub async fn get_transaction(&self, hash: &str) -> Result<GetTransactionResponse, EngineError> {
        let result = self
            .transport
            .call("getTransaction", json!({ "hash": hash }))
            .await?;

        serde_json::from_value(result)
            .map_err(|e| EngineError::Internal(format!("parse getTransaction result: {e}")))
    }

    #[allow(dead_code)]
    pub async fn get_latest_ledger(&self) -> Result<u32, EngineError> {
        let result = self.transport.call("getLatestLedger", json!({})).await?;
        result["sequence"]
            .as_u64()
            .map(|n| n as u32)
            .ok_or_else(|| EngineError::Internal("missing ledger sequence".into()))
    }
}
