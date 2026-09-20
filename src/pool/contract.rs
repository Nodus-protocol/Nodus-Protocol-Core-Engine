use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::config::{Network, PoolConfig};
use crate::pool::abi::PoolFunction;
use crate::pool::prepare::{self, PrepareParams, PreparedTransaction};
use crate::pool::provider::{ProviderPool, MAX_LEDGER_AGE_SECS};
use crate::pool::{math, soroban::SorobanRpc, xdr};
use crate::utils::{AssetId, EngineError, RationalPrice};

const CACHE_TTL: Duration = Duration::from_secs(30);

fn i128_to_u128(v: i128) -> Option<u128> {
    u128::try_from(v).ok()
}

/// Current on-chain reserve state. `token_0`/`token_1` carry the full
/// canonical [`AssetId`] — ordering is fixed by the deployed contract.
#[derive(Debug, Clone, Serialize)]
pub struct PoolReserves {
    pub reserve_0: u128,
    pub reserve_1: u128,
    pub token_0: AssetId,
    pub token_1: AssetId,
    pub lp_total_supply: u128,
    pub timestamp_last: u64,
}

/// AMM price quote. All amounts are integer base units.
///
/// `effective_price` is the exact rational `amount_in / amount_out`
/// (GCD-reduced, no floating-point encoding).
/// Rounding: `get_amount_out` floors (integer division) — the pool always
/// retains any fractional base unit, matching the audited smart contract.
#[derive(Debug, Clone, Serialize)]
pub struct PriceQuote {
    pub amount_in: u128,
    pub amount_out: u128,
    pub token_in: AssetId,
    pub token_out: AssetId,
    /// Protocol fee in basis points (30 = 0.30 %).
    pub fee_bps: u64,
    pub price_impact_bps: u64,
    /// Exact rational `amount_in / amount_out` as `{numerator, denominator}`
    /// decimal strings. Scale by token decimals for human-readable price.
    pub effective_price: RationalPrice,
    /// Ledger sequence from which reserves were read.
    pub source_ledger: u32,
    /// Freshness age in seconds of the source ledger close time.
    pub freshness_seconds: u64,
}

struct CachedReserves {
    network: Network,
    contract_id: String,
    source_ledger: u32,
    data: PoolReserves,
    fetched_at: Instant,
    ledger_close_time: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmitResult {
    pub hash: String,
    pub send_status: String,
    pub final_status: String,
    pub result_xdr: Option<String>,
    pub polled_attempts: u32,
}

pub struct ContractClient {
    rpc: SorobanRpc,
    providers: Option<Arc<ProviderPool>>,
    pool: PoolConfig,
    network: Network,
    cache: RwLock<Option<CachedReserves>>,
}

impl ContractClient {
    pub fn new(rpc: SorobanRpc, pool: &PoolConfig, network: Network) -> Self {
        let valid_urls: Vec<String> = if !pool.soroban_rpc_urls.is_empty() {
            pool.soroban_rpc_urls
                .iter()
                .filter(|u| !u.is_empty() && !u.contains("unused.invalid"))
                .cloned()
                .collect()
        } else if !pool.soroban_rpc_url.is_empty()
            && !pool.soroban_rpc_url.contains("unused.invalid")
        {
            vec![pool.soroban_rpc_url.clone()]
        } else {
            Vec::new()
        };

        let providers = if !valid_urls.is_empty() {
            Some(Arc::new(ProviderPool::new(
                &valid_urls,
                network,
                pool.provider_divergence_max_ledgers,
            )))
        } else {
            None
        };

        Self {
            rpc,
            providers,
            pool: pool.clone(),
            network,
            cache: RwLock::new(None),
        }
    }

    #[allow(dead_code)]
    pub fn with_provider_pool(
        providers: Arc<ProviderPool>,
        pool: &PoolConfig,
        network: Network,
    ) -> Self {
        Self {
            rpc: SorobanRpc::new(&providers.primary_endpoint()),
            providers: Some(providers),
            pool: pool.clone(),
            network,
            cache: RwLock::new(None),
        }
    }

    pub fn providers(&self) -> Option<Arc<ProviderPool>> {
        self.providers.clone()
    }

    #[allow(dead_code)]
    pub fn provider_pool(&self) -> Option<Arc<ProviderPool>> {
        self.providers.clone()
    }

    fn contract_id(&self) -> &str {
        &self.pool.contract_id
    }

    pub async fn get_reserves(&self) -> Result<PoolReserves, EngineError> {
        self.get_reserves_with_freshness()
            .await
            .map(|(reserves, _, _)| reserves)
    }

    pub async fn get_reserves_with_freshness(
        &self,
    ) -> Result<(PoolReserves, u32, u64), EngineError> {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        {
            let guard = self.cache.read().await;
            if let Some(ref c) = *guard {
                if c.network == self.network
                    && c.contract_id == self.pool.contract_id
                    && c.fetched_at.elapsed() < CACHE_TTL
                {
                    let age = now_unix.saturating_sub(c.ledger_close_time);
                    if age <= MAX_LEDGER_AGE_SECS {
                        crate::observability::gauge(
                            "nodus_quote_cache_hit_age_seconds",
                            c.fetched_at.elapsed().as_secs_f64(),
                        );
                        crate::observability::gauge(
                            "nodus_quote_source_ledger",
                            c.source_ledger as f64,
                        );
                        crate::observability::gauge("nodus_quote_age_seconds", age as f64);
                        return Ok((c.data.clone(), c.source_ledger, age));
                    }
                }
            }
        }

        let (reserves, source_ledger, close_time) = self.fetch_reserves_with_metadata().await?;
        let age = now_unix.saturating_sub(close_time);

        crate::observability::gauge("nodus_quote_source_ledger", source_ledger as f64);
        crate::observability::gauge("nodus_quote_age_seconds", age as f64);

        let mut guard = self.cache.write().await;
        *guard = Some(CachedReserves {
            network: self.network,
            contract_id: self.pool.contract_id.clone(),
            source_ledger,
            data: reserves.clone(),
            fetched_at: Instant::now(),
            ledger_close_time: close_time,
        });

        Ok((reserves, source_ledger, age))
    }

    /// Token direction is resolved by canonical key (contract address or
    /// `"native"`) — never by bare symbol string.
    pub async fn get_quote(
        &self,
        amount_in: u128,
        token_in: &AssetId,
    ) -> Result<PriceQuote, EngineError> {
        let (reserves, source_ledger, freshness_seconds) =
            self.get_reserves_with_freshness().await?;

        let key_in = token_in.canonical_key();
        let key_0 = reserves.token_0.canonical_key();
        let key_1 = reserves.token_1.canonical_key();

        let (reserve_in, reserve_out, token_out) = if key_in == key_0 {
            (
                reserves.reserve_0,
                reserves.reserve_1,
                reserves.token_1.clone(),
            )
        } else if key_in == key_1 {
            (
                reserves.reserve_1,
                reserves.reserve_0,
                reserves.token_0.clone(),
            )
        } else {
            return Err(EngineError::InvalidRequest(format!(
                "asset '{}' (key: '{key_in}') is not in this pool \
                 (pool keys: '{key_0}', '{key_1}')",
                token_in.symbol
            )));
        };

        let amount_out = math::get_amount_out(amount_in, reserve_in, reserve_out)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

        let price_impact = math::price_impact_bps(amount_in, reserve_in);
        let effective_price = if amount_out > 0 {
            RationalPrice::new(amount_in, amount_out)
        } else {
            RationalPrice::zero()
        };

        Ok(PriceQuote {
            amount_in,
            amount_out,
            token_in: token_in.clone(),
            token_out,
            fee_bps: ((math::FEE_DENOMINATOR - math::FEE_NUMERATOR) * 10) as u64,
            price_impact_bps: price_impact,
            effective_price,
            source_ledger,
            freshness_seconds,
        })
    }

    /// **Known limitation:** queries `"LpBalance"` on the pool contract's
    /// own instance storage; the deployed contract stores LP balances on a
    /// separate LP token contract, so this always returns 0.
    pub async fn lp_balance(&self, address: &str) -> Result<u128, EngineError> {
        let lp_token = self.lp_token_address().await?;
        let key = xdr::sepal41_balance_key(address)?;
        let key_xdr = xdr::contract_persistent_ledger_key(&lp_token, key)?;
        let entries = self.rpc.get_ledger_entries(vec![key_xdr]).await?;
        let entry = match entries.first() {
            Some(entry) => entry,
            // No entry means the holder has never received LP tokens (0).
            None => return Ok(0),
        };
        let val = xdr::decode_contract_data_val(&entry.xdr)?;
        i128_to_u128(xdr::scval_to_i128(&val)?).ok_or_else(|| {
            EngineError::Internal("LP token balance is negative or overflowed u128".into())
        })
    }

    /// Resolves the pool's own tracked LP token contract address
    /// (`DataKey::LpToken`) from the pool's typed instance storage.
    async fn lp_token_address(&self) -> Result<String, EngineError> {
        let map = self.fetch_pool_instance_map().await?;
        let lp_token_addr = xdr::instance_address(&map, "LpToken")?;
        Ok(xdr::address_to_string(&lp_token_addr))
    }

    /// Reads the `TotalSupply` instance value from a specific LP token
    /// contract.
    async fn read_lp_total_supply(&self, lp_token: &str) -> Result<u128, EngineError> {
        let key_xdr = xdr::contract_instance_ledger_key(lp_token)?;
        let entries = self.rpc.get_ledger_entries(vec![key_xdr]).await?;
        let entry = entries.first().ok_or_else(|| {
            EngineError::NotFound(format!("LP token contract {lp_token} instance not found"))
        })?;
        let map = xdr::decode_instance_storage(&entry.xdr)?;
        let supply = xdr::instance_i128(&map, "TotalSupply")?;
        i128_to_u128(supply).ok_or_else(|| {
            EngineError::Internal("LP token total supply is negative or overflowed u128".into())
        })
    }

    pub async fn prepare_swap(
        &self,
        to: &str,
        amount_0_out: u128,
        amount_1_out: u128,
        params: PrepareParams,
    ) -> Result<PreparedTransaction, EngineError> {
        let deadline = params.deadline;
        let args = prepare::encode_swap_args(to, amount_0_out, amount_1_out, deadline)?;
        prepare::prepare(
            &self.rpc,
            &self.pool,
            &self.network,
            PoolFunction::Swap,
            args,
            params,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_add_liquidity(
        &self,
        from: &str,
        to: &str,
        amount_0_desired: u128,
        amount_1_desired: u128,
        amount_0_min: u128,
        amount_1_min: u128,
        params: PrepareParams,
    ) -> Result<PreparedTransaction, EngineError> {
        let deadline = params.deadline;
        let args = prepare::encode_add_liquidity_args(
            from,
            to,
            amount_0_desired,
            amount_1_desired,
            amount_0_min,
            amount_1_min,
            deadline,
        )?;
        prepare::prepare(
            &self.rpc,
            &self.pool,
            &self.network,
            PoolFunction::AddLiquidity,
            args,
            params,
        )
        .await
    }

    pub async fn prepare_remove_liquidity(
        &self,
        from: &str,
        to: &str,
        liquidity: u128,
        amount_0_min: u128,
        amount_1_min: u128,
        params: PrepareParams,
    ) -> Result<PreparedTransaction, EngineError> {
        let deadline = params.deadline;
        let args = prepare::encode_remove_liquidity_args(
            from,
            to,
            liquidity,
            amount_0_min,
            amount_1_min,
            deadline,
        )?;
        prepare::prepare(
            &self.rpc,
            &self.pool,
            &self.network,
            PoolFunction::RemoveLiquidity,
            args,
            params,
        )
        .await
    }

    pub async fn validate_transaction(
        &self,
        xdr: &str,
        declared_network: Network,
        source: &str,
        now: u64,
    ) -> Result<prepare::ReviewSummary, EngineError> {
        let account_key = xdr::account_ledger_key(source)?;
        let entries = self.rpc.get_ledger_entries(vec![account_key]).await?;
        let expected_sequence = match entries.first() {
            Some(entry) => Some(prepare::decode_account_sequence(&entry.xdr)?),
            None => None,
        };
        prepare::validate(
            xdr,
            &prepare::ValidateContext {
                network: self.network,
                declared_network,
                contract_id: self.pool.contract_id.clone(),
                fee_ceiling_stroops: self.pool.fee_ceiling_stroops,
                now,
                expected_sequence,
                current_ledger: None,
                simulation_ledger: None,
            },
        )
    }

    pub async fn submit_transaction(&self, signed_xdr: &str) -> Result<SubmitResult, EngineError> {
        let tx_hash = derive_tx_hash_from_xdr(signed_xdr)?;
        tracing::info!(%tx_hash, "initiating verified transaction submission");

        let sent = if let Some(ref pool) = self.providers {
            pool.send_transaction(signed_xdr).await?
        } else {
            self.rpc.send_transaction(signed_xdr).await?
        };

        if sent.status != "PENDING" && sent.status != "DUPLICATE" {
            return Err(EngineError::PreconditionFailed(format!(
                "sendTransaction returned {}: {}",
                sent.status,
                sent.error_result_xdr.unwrap_or_default()
            )));
        }

        const MAX_POLLS: u32 = 10;
        const POLL_DELAY: Duration = Duration::from_secs(2);

        for attempt in 1..=MAX_POLLS {
            tokio::time::sleep(POLL_DELAY).await;
            let got = if let Some(ref pool) = self.providers {
                pool.get_transaction(&sent.hash).await?
            } else {
                self.rpc.get_transaction(&sent.hash).await?
            };

            if got.status != "NOT_FOUND" {
                return Ok(SubmitResult {
                    hash: sent.hash,
                    send_status: sent.status,
                    final_status: got.status,
                    result_xdr: got.result_xdr,
                    polled_attempts: attempt,
                });
            }
        }

        // Before returning NOT_FOUND or deciding on resend, check ALL approved providers
        if let Some(ref pool) = self.providers {
            let consensus_ledger = pool.latest_consensus_ledger();
            let _ = pool
                .check_all_approved_unanimous_not_found(&sent.hash, consensus_ledger)
                .await;
        }

        Ok(SubmitResult {
            hash: sent.hash,
            send_status: sent.status,
            final_status: "NOT_FOUND".to_string(),
            result_xdr: None,
            polled_attempts: MAX_POLLS,
        })
    }

    async fn fetch_reserves_with_metadata(&self) -> Result<(PoolReserves, u32, u64), EngineError> {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let (map, source_ledger, close_time) = if let Some(ref pool) = self.providers {
            pool.read_reserves_with_quorum(self.contract_id()).await?
        } else {
            let map = self.fetch_pool_instance_map().await?;
            let ledger = self.rpc.get_latest_ledger().await.unwrap_or(0);
            (map, ledger, now_unix)
        };

        let reserve_0 = xdr::instance_i128(&map, "Reserve0").and_then(|v| {
            i128_to_u128(v).ok_or_else(|| {
                EngineError::Internal("Reserve0 is negative or overflowed u128".into())
            })
        })?;
        let reserve_1 = xdr::instance_i128(&map, "Reserve1").and_then(|v| {
            i128_to_u128(v).ok_or_else(|| {
                EngineError::Internal("Reserve1 is negative or overflowed u128".into())
            })
        })?;
        let timestamp_last = xdr::instance_u64(&map, "TimestampLast")?;

        let lp_token = xdr::address_to_string(&xdr::instance_address(&map, "LpToken")?);
        let lp_total_supply = self.read_lp_total_supply(&lp_token).await?;

        Ok((
            PoolReserves {
                reserve_0,
                reserve_1,
                token_0: self.pool.token_0.clone(),
                token_1: self.pool.token_1.clone(),
                lp_total_supply,
                timestamp_last,
            },
            source_ledger,
            close_time,
        ))
    }

    /// Reads and decodes the pool contract's typed instance storage map
    /// exactly once per call, emitting the source-ledger gauge.
    async fn fetch_pool_instance_map(&self) -> Result<xdr::ScMap, EngineError> {
        let key = xdr::contract_instance_ledger_key(self.contract_id())?;
        let entries = self.rpc.get_ledger_entries(vec![key]).await?;
        if entries.is_empty() {
            return Err(EngineError::NotFound("contract instance not found".into()));
        }
        if let Some(ledger) = entries[0].last_modified {
            crate::observability::gauge("nodus_quote_source_ledger", ledger as f64);
        }
        crate::observability::gauge("nodus_provider_divergence_ledgers", 0.0);
        xdr::decode_instance_storage(&entries[0].xdr)
    }

    #[allow(dead_code)]
    fn lp_balance_key_xdr(&self, address: &str) -> Result<String, EngineError> {
        let contract_bytes = parse_contract_id(self.contract_id())?;
        let addr_bytes = parse_contract_id(address)?;
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&contract_bytes);
        buf.extend_from_slice(&11u32.to_be_bytes());
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&7u32.to_be_bytes());
        let sym = b"LpBalance";
        buf.extend_from_slice(&(sym.len() as u32).to_be_bytes());
        buf.extend_from_slice(sym);
        pad4(&mut buf, sym.len());
        buf.extend_from_slice(&6u32.to_be_bytes());
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&addr_bytes);
        buf.extend_from_slice(&1u32.to_be_bytes());
        Ok(B64.encode(&buf))
    }
}

#[allow(dead_code)]
fn parse_contract_id(id: &str) -> Result<Vec<u8>, EngineError> {
    let clean = id.trim_start_matches("C");
    hex::decode(clean)
        .or_else(|_| {
            B64.decode(id)
                .map_err(|_| EngineError::InvalidRequest(format!("invalid contract id: {id}")))
        })
        .and_then(|b| {
            if b.len() == 32 {
                Ok(b)
            } else {
                Err(EngineError::InvalidRequest(format!(
                    "contract id must be 32 bytes: {id}"
                )))
            }
        })
}

#[allow(dead_code)]
fn pad4(buf: &mut Vec<u8>, len: usize) {
    let rem = len % 4;
    if rem != 0 {
        buf.extend(std::iter::repeat_n(0u8, 4 - rem));
    }
}

#[allow(dead_code)]
fn parse_i128_from_xdr(xdr: &str) -> Result<u128, EngineError> {
    let bytes = B64
        .decode(xdr)
        .map_err(|e| EngineError::Internal(format!("decode xdr: {e}")))?;
    if bytes.len() < 20 {
        return Ok(0);
    }
    let hi = i64::from_be_bytes(bytes[4..12].try_into().unwrap_or([0; 8]));
    let lo = u64::from_be_bytes(bytes[12..20].try_into().unwrap_or([0; 8]));
    if hi < 0 {
        return Ok(0);
    }
    Ok((hi as u128) << 64 | lo as u128)
}

#[allow(dead_code)]
fn parse_instance_storage(
    xdr: &str,
    token_0: &AssetId,
    token_1: &AssetId,
) -> Result<PoolReserves, EngineError> {
    let bytes = B64
        .decode(xdr)
        .map_err(|e| EngineError::Internal(format!("decode instance xdr: {e}")))?;

    let reserve_0 = extract_i128_by_key(&bytes, b"Reserve0").unwrap_or(0);
    let reserve_1 = extract_i128_by_key(&bytes, b"Reserve1").unwrap_or(0);
    let lp_supply = extract_i128_by_key(&bytes, b"LpTotalSup").unwrap_or(0);
    let ts = extract_u64_by_key(&bytes, b"TimestampL").unwrap_or(0);

    Ok(PoolReserves {
        reserve_0,
        reserve_1,
        token_0: token_0.clone(),
        token_1: token_1.clone(),
        lp_total_supply: lp_supply,
        timestamp_last: ts,
    })
}

#[allow(dead_code)]
fn extract_i128_by_key(buf: &[u8], key: &[u8]) -> Option<u128> {
    let pos = buf.windows(key.len()).position(|w| w == key)?;
    let start = pos + key.len();
    if start + 16 > buf.len() {
        return None;
    }
    let hi = i64::from_be_bytes(buf[start..start + 8].try_into().ok()?);
    let lo = u64::from_be_bytes(buf[start + 8..start + 16].try_into().ok()?);
    if hi < 0 {
        return Some(0);
    }
    Some((hi as u128) << 64 | lo as u128)
}

#[allow(dead_code)]
fn extract_u64_by_key(buf: &[u8], key: &[u8]) -> Option<u64> {
    let pos = buf.windows(key.len()).position(|w| w == key)?;
    let start = pos + key.len();
    if start + 8 > buf.len() {
        return None;
    }
    Some(u64::from_be_bytes(buf[start..start + 8].try_into().ok()?))
}

pub fn derive_tx_hash_from_xdr(signed_xdr: &str) -> Result<String, EngineError> {
    use base64::Engine;
    use sha2::Digest;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(signed_xdr.trim())
        .map_err(|e| EngineError::InvalidRequest(format!("invalid base64 XDR: {e}")))?;

    let hash = sha2::Sha256::digest(&bytes);
    Ok(hex::encode(hash))
}
