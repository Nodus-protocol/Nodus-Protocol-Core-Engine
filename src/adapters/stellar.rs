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

    pub fn mainnet() -> Self {
        Self::new("https://horizon.stellar.org")
    }

    pub fn testnet() -> Self {
        Self::new("https://horizon-testnet.stellar.org")
    }
}

#[async_trait]
impl ChainAdapter for StellarAdapter {
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError> {
        validation::stellar_address(&payment.sender)?;
        validation::stellar_address(&payment.recipient)?;
        validation::amount(payment.amount)?;

        tracing::info!(
            payment_id = %payment.id,
            from  = %payment.sender,
            to    = %payment.recipient,
            amount = payment.amount,
            token  = %payment.token,
            "submitting to Stellar"
        );

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

        match self.client.get(&url).send().await {
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
            Err(e) => {
                tracing::warn!("Horizon fee_stats unavailable: {e}");
                Ok(FeeEstimate::default())
            }
            _ => Ok(FeeEstimate::default()),
        }
    }

    async fn is_confirmed(&self, tx_hash: &str) -> Result<bool, EngineError> {
        let url = format!("{}/transactions/{}", self.horizon_url, tx_hash);
        match self.client.get(&url).send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) => Err(EngineError::NetworkError(e.to_string())),
        }
    }

    fn name(&self) -> &'static str {
        "stellar"
    }
}
