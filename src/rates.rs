use crate::utils::{AssetId, EngineError, RationalPrice};
use dashmap::DashMap;
use serde::Serialize;
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(60);
const COINGECKO_BASE: &str = "https://api.coingecko.com/api/v3";

/// Cached USD price stored as an exact rational (numerator/denominator in
/// micro-USD, i.e. denominator = 1_000_000) to avoid f64 rounding at rest.
struct CachedRate {
    /// Numerator of price in units of 1e-6 USD (micro-USD).
    usd_price_micro_numerator: u64,
    fetched_at: Instant,
}

pub struct RateService {
    /// Key: canonical asset key (contract address or "native").
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

    /// Returns the USD price of `asset` as an exact [`RationalPrice`]
    /// with denominator 1_000_000 (micro-USD precision).
    /// Cache key is the asset's canonical key, not its symbol.
    pub async fn usd_price(&self, asset: &AssetId) -> Result<RationalPrice, EngineError> {
        let cache_key = asset.canonical_key();

        if let Some(entry) = self.cache.get(&cache_key) {
            if entry.fetched_at.elapsed() < CACHE_TTL {
                return Ok(RationalPrice::new(
                    entry.usd_price_micro_numerator as u128,
                    1_000_000,
                ));
            }
        }

        let micro = self.fetch_from_coingecko(asset).await?;
        self.cache.insert(
            cache_key,
            CachedRate {
                usd_price_micro_numerator: micro,
                fetched_at: Instant::now(),
            },
        );
        Ok(RationalPrice::new(micro as u128, 1_000_000))
    }

    pub async fn rates_for(&self, assets: &[&AssetId]) -> Vec<TokenRate> {
        let mut result = Vec::new();
        for &asset in assets {
            let rate = self.usd_price(asset).await;
            let available = rate.is_ok();
            result.push(TokenRate {
                asset: asset.clone(),
                usd_price: rate.unwrap_or_else(|_| RationalPrice::zero()),
                available,
            });
        }
        result
    }

    async fn fetch_from_coingecko(&self, asset: &AssetId) -> Result<u64, EngineError> {
        let id = coingecko_id(asset).ok_or_else(|| {
            EngineError::NotFound(format!(
                "no CoinGecko mapping for asset '{}' (key: '{}')",
                asset.symbol,
                asset.canonical_key()
            ))
        })?;

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

        // CoinGecko returns a float; convert to micro-USD integer to avoid
        // storing f64 in the cache or in API responses.
        let price_f64 = body[id]["usd"]
            .as_f64()
            .ok_or_else(|| EngineError::NotFound(format!("no price data for '{}'", asset.symbol)))?;

        // Round to nearest micro-USD.
        Ok((price_f64 * 1_000_000.0).round() as u64)
    }
}

/// Maps a canonical AssetId to its CoinGecko price feed ID.
/// Returns `None` for unknown assets so the caller can surface a clear error
/// instead of silently defaulting to XLM pricing.
fn coingecko_id(asset: &AssetId) -> Option<&'static str> {
    // Match on canonical key first (contract address), then fall back to
    // upper-cased symbol for well-known assets where no contract is set.
    let key = asset.canonical_key();
    // Well-known mainnet SAC contract addresses.
    match key.as_str() {
        // USDC SAC on Stellar mainnet
        "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75" => return Some("usd-coin"),
        "native" => return Some("stellar"),
        _ => {}
    }
    // Symbol fallback for native / testnet assets without a contract.
    match asset.symbol.to_uppercase().as_str() {
        "XLM" => Some("stellar"),
        "USDC" => Some("usd-coin"),
        "BTC" | "WBTC" => Some("bitcoin"),
        "ETH" | "WETH" => Some("ethereum"),
        _ => None,
    }
}

#[derive(Serialize)]
pub struct TokenRate {
    /// Full canonical asset identity — never just a bare symbol.
    pub asset: AssetId,
    /// Exact rational USD price with denominator 1_000_000.
    /// e.g. `{ "numerator": "123456", "denominator": "1000000" }` = $0.123456
    pub usd_price: RationalPrice,
    pub available: bool,
}
