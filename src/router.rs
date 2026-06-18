use crate::adapters::ChainAdapter;
use crate::utils::{EngineError, FeeEstimate, Urgency};
use std::sync::Arc;

pub struct RouteOption {
    pub adapter: Arc<dyn ChainAdapter>,
    pub fee_stroops: u64,
    pub estimated_seconds: u32,
    pub score: f64,
}

pub struct Router {
    adapters: Vec<Arc<dyn ChainAdapter>>,
}

impl Router {
    pub fn new(adapters: Vec<Arc<dyn ChainAdapter>>) -> Self {
        Self { adapters }
    }

    pub async fn select(&self, urgency: &Urgency) -> Result<RouteOption, EngineError> {
        if self.adapters.is_empty() {
            return Err(EngineError::Internal("no chain adapters registered".into()));
        }

        let mut candidates: Vec<RouteOption> = Vec::new();

        for adapter in &self.adapters {
            let fees = match adapter.fee_estimate().await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(chain = adapter.name(), error = %e, "skipping degraded adapter");
                    continue;
                }
            };

            let (fee, secs) = fee_for_urgency(&fees, urgency);
            let score = score(fee, secs, urgency);

            candidates.push(RouteOption {
                adapter: adapter.clone(),
                fee_stroops: fee,
                estimated_seconds: secs,
                score,
            });
        }

        if candidates.is_empty() {
            return Err(EngineError::AdapterError(
                "all chain adapters are unavailable".into(),
            ));
        }

        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(candidates.remove(0))
    }

    pub async fn all_fees(&self) -> Vec<ChainFees> {
        let mut result = Vec::new();
        for adapter in &self.adapters {
            let fees = adapter.fee_estimate().await.unwrap_or_default();
            result.push(ChainFees {
                chain: adapter.name(),
                fees,
                available: true,
            });
        }
        result
    }
}

fn fee_for_urgency(fees: &FeeEstimate, urgency: &Urgency) -> (u64, u32) {
    match urgency {
        Urgency::Standard => (fees.standard_stroops, fees.standard_seconds),
        Urgency::Fast => (fees.fast_stroops, fees.fast_seconds),
        Urgency::Urgent => (fees.urgent_stroops, fees.urgent_seconds),
    }
}

fn score(fee_stroops: u64, secs: u32, urgency: &Urgency) -> f64 {
    let latency_weight = match urgency {
        Urgency::Urgent => 5.0,
        Urgency::Fast => 2.0,
        Urgency::Standard => 0.5,
    };
    let fee_cost = fee_stroops as f64 / 100.0;
    let latency_cost = secs as f64 * latency_weight;
    -(fee_cost + latency_cost)
}

#[derive(serde::Serialize)]
pub struct ChainFees {
    pub chain: &'static str,
    pub fees: FeeEstimate,
    pub available: bool,
}
