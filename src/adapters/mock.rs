use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::adapters::ChainAdapter;
use crate::utils::{EngineError, FeeEstimate, Payment};

pub struct MockAdapter {
    pub name: &'static str,
    pub submit_count: Arc<AtomicU32>,
    pub should_fail: bool,
}

impl MockAdapter {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            submit_count: Arc::new(AtomicU32::new(0)),
            should_fail: false,
        }
    }

    pub fn failing(name: &'static str) -> Self {
        Self {
            should_fail: true,
            ..Self::new(name)
        }
    }

    pub fn submit_count(&self) -> u32 {
        self.submit_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ChainAdapter for MockAdapter {
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError> {
        self.submit_count.fetch_add(1, Ordering::SeqCst);
        if self.should_fail {
            return Err(EngineError::AdapterError("mock failure".into()));
        }
        Ok(format!("mock_tx_{}", &payment.id[..8]))
    }

    async fn fee_estimate(&self) -> Result<FeeEstimate, EngineError> {
        if self.should_fail {
            return Err(EngineError::AdapterError("mock fee failure".into()));
        }
        Ok(FeeEstimate::default())
    }

    async fn is_confirmed(&self, _tx_hash: &str) -> Result<bool, EngineError> {
        Ok(!self.should_fail)
    }

    fn name(&self) -> &'static str {
        self.name
    }
}
