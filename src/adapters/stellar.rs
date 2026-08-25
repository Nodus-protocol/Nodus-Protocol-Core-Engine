use crate::adapters::ChainAdapter;
use crate::utils::{EngineError, FeeEstimate, Payment};
use crate::validation;
use async_trait::async_trait;

pub struct StellarAdapter {
    pub(crate) horizon_url: String,
    client: reqwest::Client,
}

impl StellarAdapter {
    pub fn new(horizon_url: &str) -> Self {
        Self {
            horizon_url: horizon_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

#[async_trait]
impl ChainAdapter for StellarAdapter {
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError> {
        validation::stellar_address(&payment.sender)?;
        validation::stellar_address(&payment.recipient)?;
        validation::amount(payment.amount)?;

        tracing::info!(payment_id = %payment.id, "submitting to Stellar");

        // Derives a deterministic mock tx hash from the payment ID.
        // Replace with real XDR construction + POST /transactions to Horizon.
        let tx_hash = format!(
            "{:0>64}",
            payment
                .id
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        );
        Ok(tx_hash)
    }

    async fn fee_estimate(&self) -> Result<FeeEstimate, EngineError> {
        let url = format!("{}/fee_stats", self.horizon_url);

        match crate::observability::propagate(self.client.get(&url))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let stats: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| EngineError::NetworkError(e.to_string()))?;

                let parse = |key: &str, sub: &str| -> u64 {
                    stats[key][sub]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(match sub {
                            "p50" => 100,
                            "p75" => 250,
                            "p90" => 500,
                            _ => 100,
                        })
                };

                Ok(FeeEstimate {
                    standard_stroops: parse("fee_charged", "p50"),
                    fast_stroops: parse("fee_charged", "p75"),
                    urgent_stroops: parse("fee_charged", "p90"),
                    standard_seconds: 5,
                    fast_seconds: 3,
                    urgent_seconds: 1,
                })
            }
            Err(e) => Err(EngineError::NetworkError(e.to_string())),
            Ok(resp) => Err(EngineError::NetworkError(format!(
                "horizon returned {}",
                resp.status()
            ))),
        }
    }

    async fn is_confirmed(&self, tx_hash: &str) -> Result<bool, EngineError> {
        let url = format!("{}/transactions/{}", self.horizon_url, tx_hash);
        match crate::observability::propagate(self.client.get(&url))
            .send()
            .await
        {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) => Err(EngineError::NetworkError(e.to_string())),
        }
    }

    async fn is_ready(&self) -> bool {
        let response = match crate::observability::propagate(self.client.get(&self.horizon_url))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            _ => return false,
        };
        let body: serde_json::Value = match response.json().await {
            Ok(body) => body,
            Err(_) => return false,
        };
        let closed = body["history_latest_ledger_close_time"]
            .as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        closed.is_some_and(|time| {
            let age = chrono::Utc::now()
                .signed_duration_since(time.with_timezone(&chrono::Utc))
                .num_seconds();
            (0..=30).contains(&age)
        })
    }

    fn name(&self) -> &'static str {
        "stellar"
    }
}
