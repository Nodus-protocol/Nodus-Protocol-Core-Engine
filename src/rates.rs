use crate::utils::EngineError;
use dashmap::DashMap;
use serde::Serialize;
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(60);
const COINGECKO_BASE: &str = "https://api.coingecko.com/api/v3";

struct CachedRate {
    usd_price: f64,
    fetched_at: Instant,
}

pub struct RateService {
    cache: DashMap<String, CachedRate>,
    client: reqwest::Client,
}

impl Default for RateService {
    fn default() -> Self {
        Self::new()
    }
}

impl RateService {
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build rate service HTTP client"),
        }
    }

    pub async fn usd_price(&self, token: &str) -> Result<f64, EngineError> {
        let token_lower = token.to_lowercase();

        if let Some(entry) = self.cache.get(&token_lower) {
            if entry.fetched_at.elapsed() < CACHE_TTL {
                return Ok(entry.usd_price);
            }
        }

        let price = self.fetch_from_coingecko(&token_lower).await?;
        self.cache.insert(
            token_lower,
            CachedRate {
                usd_price: price,
                fetched_at: Instant::now(),
            },
        );
        Ok(price)
    }

    #[allow(dead_code)]
    pub async fn convert(
        &self,
        amount: u64,
        from_token: &str,
        to_token: &str,
    ) -> Result<f64, EngineError> {
        let from_usd = self.usd_price(from_token).await?;
        let to_usd = self.usd_price(to_token).await?;
        if to_usd == 0.0 {
            return Err(EngineError::InvalidRequest(format!(
                "zero price for {to_token}"
            )));
        }
        Ok((amount as f64 * from_usd) / to_usd)
    }

    pub async fn rates_for(&self, tokens: &[&str]) -> Vec<TokenRate> {
        let mut result = Vec::new();
        for &token in tokens {
            let rate = self.usd_price(token).await;
            let available = rate.is_ok();
            result.push(TokenRate {
                token: token.to_string(),
                usd_price: rate.unwrap_or(0.0),
                available,
            });
        }
        result
    }

    async fn fetch_from_coingecko(&self, token: &str) -> Result<f64, EngineError> {
        let id = coingecko_id(token);
        let url = format!("{COINGECKO_BASE}/simple/price?ids={id}&vs_currencies=usd");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| EngineError::NetworkError(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(EngineError::NetworkError(format!(
                "CoinGecko returned {}",
                resp.status()
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EngineError::NetworkError(e.to_string()))?;

        body[id]["usd"]
            .as_f64()
            .ok_or_else(|| EngineError::NotFound(format!("no price data for {token}")))
    }
}

fn coingecko_id(token: &str) -> &'static str {
    match token {
        "xlm" | "XLM" => "stellar",
        "usdc" | "USDC" => "usd-coin",
        "btc" | "BTC" => "bitcoin",
        "eth" | "ETH" => "ethereum",
        _ => "stellar",
    }
}

#[derive(Serialize)]
pub struct TokenRate {
    pub token: String,
    pub usd_price: f64,
    pub available: bool,
}
