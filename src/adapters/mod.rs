mod stellar;
pub use stellar::StellarAdapter;

pub mod mock;

use crate::utils::{EngineError, FeeEstimate, Payment};
use async_trait::async_trait;

#[async_trait]
pub trait ChainAdapter: Send + Sync {
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError>;
    async fn fee_estimate(&self) -> Result<FeeEstimate, EngineError>;
    #[allow(dead_code)]
    async fn is_confirmed(&self, tx_hash: &str) -> Result<bool, EngineError>;
    fn name(&self) -> &'static str;
}
