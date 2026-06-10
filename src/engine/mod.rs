use std::sync::Arc;
use uuid::Uuid;

use crate::adapters::ChainAdapter;
use crate::retry::{retry, RetryConfig};
use crate::router::Router;
use crate::store::PaymentStore;
use crate::utils::{now_utc, EngineError, FeeEstimate, Payment, PaymentStatus, Urgency};

pub struct Engine {
    router: Router,
    store: PaymentStore,
    retry_config: RetryConfig,
}

impl Engine {
    pub fn new(adapters: Vec<Arc<dyn ChainAdapter>>, retry_config: RetryConfig) -> Self {
        Self {
            router: Router::new(adapters),
            store: PaymentStore::new(),
            retry_config,
        }
    }

    pub async fn initiate(
        &self,
        sender: String,
        recipient: String,
        amount: u64,
        token: String,
        urgency: Urgency,
    ) -> Result<Payment, EngineError> {
        if sender.is_empty() || recipient.is_empty() {
            return Err(EngineError::InvalidRequest("sender and recipient are required".into()));
        }
        if amount == 0 {
            return Err(EngineError::InvalidRequest("amount must be > 0".into()));
        }

        let route = self.router.select(&urgency).await?;
        let now = now_utc();

        let payment = Payment {
            id: Uuid::new_v4().to_string(),
            sender,
            recipient,
            amount,
            token,
            status: PaymentStatus::Pending,
            tx_hash: None,
            fee_stroops: route.fee_stroops,
            urgency,
            error: None,
            created_at: now.clone(),
            updated_at: now,
        };

        self.store.insert(payment.clone());
        self.store.set_status(&payment.id, PaymentStatus::Processing)?;

        tracing::info!(payment_id = %payment.id, chain = route.adapter.name(), "payment processing");

        let adapter = route.adapter.clone();
        let payment_ref = payment.clone();
        let retry_cfg = self.retry_config.clone();

        match retry(&retry_cfg, || adapter.submit(&payment_ref)).await {
            Ok(tx_hash) => {
                self.store.set_confirmed(&payment.id, tx_hash.clone())?;
                tracing::info!(payment_id = %payment.id, %tx_hash, "payment confirmed");
            }
            Err(e) => {
                self.store.set_failed(&payment.id, e.to_string())?;
                tracing::warn!(payment_id = %payment.id, error = %e, "payment failed");
            }
        }

        self.store.get(&payment.id)
    }

    pub fn get(&self, id: &str) -> Result<Payment, EngineError> {
        self.store.get(id)
    }

    pub fn list(&self) -> Vec<Payment> {
        self.store.list()
    }

    pub async fn simulate(
        &self,
        sender: String,
        recipient: String,
        amount: u64,
        token: String,
        urgency: Urgency,
    ) -> Result<SimulationResult, EngineError> {
        if amount == 0 {
            return Err(EngineError::InvalidRequest("amount must be > 0".into()));
        }

        let route = self.router.select(&urgency).await?;

        Ok(SimulationResult {
            sender,
            recipient,
            amount,
            token,
            fee_stroops: route.fee_stroops,
            chain: route.adapter.name().to_string(),
            estimated_confirmation_seconds: route.estimated_seconds,
        })
    }

    pub async fn current_fees(&self) -> Vec<crate::router::ChainFees> {
        self.router.all_fees().await
    }

    pub async fn health(&self) -> HealthStatus {
        let fees = self.router.all_fees().await;
        let any_up = fees.iter().any(|f| f.available);
        HealthStatus {
            status: if any_up { "ok" } else { "degraded" },
            chains: fees.iter().map(|f| f.chain).collect(),
            payments_in_store: self.store.len(),
        }
    }
}

#[derive(serde::Serialize)]
pub struct SimulationResult {
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    pub fee_stroops: u64,
    pub chain: String,
    pub estimated_confirmation_seconds: u32,
}

#[derive(serde::Serialize)]
pub struct HealthStatus {
    pub status: &'static str,
    pub chains: Vec<&'static str>,
    pub payments_in_store: usize,
}
