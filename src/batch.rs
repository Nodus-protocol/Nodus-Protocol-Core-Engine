use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::task::JoinSet;

use crate::engine::Engine;
use crate::utils::{EngineError, Payment, Urgency};

const MAX_BATCH: usize = 100;

#[derive(Debug, Deserialize)]
pub struct BatchItem {
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    #[serde(default)]
    pub urgency: Urgency,
}

#[derive(Debug, Serialize)]
pub struct BatchItemResult {
    pub index: usize,
    pub payment: Option<Payment>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub results: Vec<BatchItemResult>,
}

pub async fn process_batch(
    engine: Arc<Engine>,
    items: Vec<BatchItem>,
) -> Result<BatchResult, EngineError> {
    if items.is_empty() {
        return Err(EngineError::InvalidRequest(
            "batch must have at least 1 item".into(),
        ));
    }
    if items.len() > MAX_BATCH {
        return Err(EngineError::InvalidRequest(format!(
            "batch exceeds maximum of {MAX_BATCH} items"
        )));
    }

    let total = items.len();
    let mut set: JoinSet<(usize, Result<Payment, EngineError>)> = JoinSet::new();

    for (i, item) in items.into_iter().enumerate() {
        let eng = engine.clone();
        set.spawn(async move {
            let result = eng
                .initiate(
                    item.sender,
                    item.recipient,
                    item.amount,
                    item.token,
                    item.urgency,
                )
                .await;
            (i, result)
        });
    }

    let mut results: Vec<BatchItemResult> = Vec::with_capacity(total);
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    while let Some(Ok((index, outcome))) = set.join_next().await {
        match outcome {
            Ok(payment) => {
                succeeded += 1;
                results.push(BatchItemResult {
                    index,
                    payment: Some(payment),
                    error: None,
                });
            }
            Err(e) => {
                failed += 1;
                results.push(BatchItemResult {
                    index,
                    payment: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    results.sort_by_key(|r| r.index);

    Ok(BatchResult {
        total,
        succeeded,
        failed,
        results,
    })
}
