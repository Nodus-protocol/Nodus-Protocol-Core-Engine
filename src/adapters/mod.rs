use async_trait::async_trait;
use crate::utils::{EngineError, FeeEstimate, Payment};

// ── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ChainAdapter: Send + Sync {
    /// Submit a payment to the underlying chain. Returns the transaction hash.
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError>;
    /// Fetch current fee estimates from the chain.
    async fn fee_estimate(&self) -> Result<FeeEstimate, EngineError>;
    /// Check whether a submitted transaction has been confirmed on-chain.
    async fn is_confirmed(&self, tx_hash: &str) -> Result<bool, EngineError>;
    /// Human-readable name for this adapter (used in logs and health checks).
    fn name(&self) -> &'static str;
}

// ── Stellar adapter ──────────────────────────────────────────────────────────

pub struct StellarAdapter {
    horizon_url: String,
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

    /// Default Stellar Horizon URL for mainnet.
    pub fn mainnet() -> Self {
        Self::new("https://horizon.stellar.org")
    }

    /// Stellar Horizon URL for testnet.
    pub fn testnet() -> Self {
        Self::new("https://horizon-testnet.stellar.org")
    }
}

#[async_trait]
impl ChainAdapter for StellarAdapter {
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError> {
        // Production: construct XDR, sign with engine keypair, submit via
        // POST /transactions. Here we validate the addresses and simulate success.
        validate_stellar_address(&payment.sender)?;
        validate_stellar_address(&payment.recipient)?;

        if payment.amount == 0 {
            return Err(EngineError::InvalidRequest("amount must be > 0".into()));
        }

        tracing::info!(
            payment_id = %payment.id,
            from       = %payment.sender,
            to         = %payment.recipient,
            amount     = payment.amount,
            token      = %payment.token,
            "submitting to Stellar"
        );

        // Simulate a transaction hash derived from the payment ID.
        let tx_hash = format!(
            "{:0>64}",
            payment.id.chars().filter(|c| c.is_alphanumeric()).collect::<String>()
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
                        .unwrap_or_else(|| match sub {
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
                tracing::warn!("fee_stats request failed: {e}, using defaults");
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

// ── Helpers ──────────────────────────────────────────────────────────────────

fn validate_stellar_address(addr: &str) -> Result<(), EngineError> {
    if addr.len() == 56 && addr.starts_with('G') && addr.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(EngineError::InvalidRequest(format!(
            "invalid Stellar address: {addr}"
        )))
    }
}
