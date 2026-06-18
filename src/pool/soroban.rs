use crate::utils::EngineError;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct SorobanRpc {
    endpoint: String,
    client: reqwest::Client,
    id: AtomicU64,
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

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct SimulateResult {
    pub results: Option<Vec<SimulateResultItem>>,
    pub error: Option<String>,
    #[serde(rename = "latestLedger")]
    pub latest_ledger: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct SimulateResultItem {
    pub xdr: String,
}

impl SorobanRpc {
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

    fn next_id(&self) -> u64 {
        self.id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn get_ledger_entries(
        &self,
        keys: Vec<String>,
    ) -> Result<Vec<LedgerEntry>, EngineError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "getLedgerEntries",
            "params": { "keys": keys }
        });

        let resp: Value = self.rpc(body).await?;

        let entries = resp["result"]["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| serde_json::from_value(e).ok())
            .collect();

        Ok(entries)
    }

    #[allow(dead_code)]
    pub async fn simulate_transaction(&self, xdr: &str) -> Result<SimulateResult, EngineError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "simulateTransaction",
            "params": { "transaction": xdr }
        });

        let resp: Value = self.rpc(body).await?;

        serde_json::from_value(resp["result"].clone())
            .map_err(|e| EngineError::Internal(format!("parse simulate result: {e}")))
    }

    #[allow(dead_code)]
    pub async fn get_latest_ledger(&self) -> Result<u32, EngineError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "getLatestLedger",
            "params": {}
        });

        let resp: Value = self.rpc(body).await?;
        resp["result"]["sequence"]
            .as_u64()
            .map(|n| n as u32)
            .ok_or_else(|| EngineError::Internal("missing ledger sequence".into()))
    }

    async fn rpc(&self, body: Value) -> Result<Value, EngineError> {
        let resp = self
            .client
            .post(&self.endpoint)
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
            return Err(EngineError::AdapterError(err.to_string()));
        }

        Ok(val)
    }
}
