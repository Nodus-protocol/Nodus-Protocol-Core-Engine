use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::config::{Network, PoolConfig};
use crate::pool::abi::PoolFunction;
use crate::pool::prepare::{self, PrepareParams, PreparedTransaction};
use crate::pool::{math, soroban::SorobanRpc, xdr};
use crate::utils::EngineError;

const CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct PoolReserves {
    pub reserve_0: u128,
    pub reserve_1: u128,
    pub token_0: String,
    pub token_1: String,
    pub lp_total_supply: u128,
    pub timestamp_last: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PriceQuote {
    pub amount_in: u128,
    pub amount_out: u128,
    pub token_in: String,
    pub token_out: String,
    pub fee_bps: u64,
    pub price_impact_bps: u64,
    pub effective_price: f64,
}

struct CachedReserves {
    data: PoolReserves,
    fetched_at: Instant,
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
    pool: PoolConfig,
    network: Network,
    cache: RwLock<Option<CachedReserves>>,
}

impl ContractClient {
    pub fn new(rpc: SorobanRpc, pool: &PoolConfig, network: Network) -> Self {
        Self {
            rpc,
            pool: pool.clone(),
            network,
            cache: RwLock::new(None),
        }
    }

    fn contract_id(&self) -> &str {
        &self.pool.contract_id
    }

    pub async fn get_reserves(&self) -> Result<PoolReserves, EngineError> {
        {
            let guard = self.cache.read().await;
            if let Some(ref c) = *guard {
                if c.fetched_at.elapsed() < CACHE_TTL {
                    crate::observability::gauge(
                        "nodus_quote_cache_hit_age_seconds",
                        c.fetched_at.elapsed().as_secs_f64(),
                    );
                    return Ok(c.data.clone());
                }
            }
        }

        let reserves = self.fetch_reserves().await?;
        crate::observability::gauge(
            "nodus_quote_age_seconds",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(reserves.timestamp_last) as f64,
        );

        let mut guard = self.cache.write().await;
        *guard = Some(CachedReserves {
            data: reserves.clone(),
            fetched_at: Instant::now(),
        });

        Ok(reserves)
    }

    pub async fn get_quote(
        &self,
        amount_in: u128,
        token_in: &str,
    ) -> Result<PriceQuote, EngineError> {
        let reserves = self.get_reserves().await?;

        let (reserve_in, reserve_out, token_out) = if token_in == reserves.token_0 {
            (
                reserves.reserve_0,
                reserves.reserve_1,
                reserves.token_1.clone(),
            )
        } else if token_in == reserves.token_1 {
            (
                reserves.reserve_1,
                reserves.reserve_0,
                reserves.token_0.clone(),
            )
        } else {
            return Err(EngineError::InvalidRequest(format!(
                "token '{token_in}' is not in this pool"
            )));
        };

        let amount_out = math::get_amount_out(amount_in, reserve_in, reserve_out)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

        let price_impact = math::price_impact_bps(amount_in, reserve_in);
        let effective_price = if amount_out > 0 {
            amount_in as f64 / amount_out as f64
        } else {
            0.0
        };

        Ok(PriceQuote {
            amount_in,
            amount_out,
            token_in: token_in.to_string(),
            token_out,
            fee_bps: 30,
            price_impact_bps: price_impact,
            effective_price,
        })
    }

    /// Reads a holder's LP-token balance from the SEP-41 LP token contract
    /// for this pool. The LP token address is itself read from the pool
    /// contract's typed instance storage (`DataKey::LpToken`), so this
    /// always queries the *actual* deployed LP token contract rather than
    /// assuming it lives on the pool — an `"LpBalance"`-on-pool assumption
    /// that previously resolved to `0` for every holder.
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

    /// Builds, simulates, and returns a ready-to-sign `swap` transaction.
    /// See [`crate::pool::prepare::prepare`] for the full pipeline.
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

    /// Builds, simulates, and returns a ready-to-sign `add_liquidity`
    /// transaction.
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

    /// Builds, simulates, and returns a ready-to-sign `remove_liquidity`
    /// transaction.
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

    /// Decodes and policy-checks an arbitrary prepared transaction XDR —
    /// the same checks [`Self::prepare_swap`] and friends run on their own
    /// output before returning it. Does a fresh live read of `source`'s
    /// on-chain sequence number so staleness is checked against current
    /// state, not a caller-supplied claim.
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
            },
        )
    }

    /// Engine-owned submission: relays an already-signed transaction
    /// envelope XDR to Soroban RPC's `sendTransaction`, then polls
    /// `getTransaction` until it leaves `NOT_FOUND` or a poll budget is
    /// exhausted. The engine never signs anything here — this is purely a
    /// relay-and-poll convenience for callers who want the engine to own
    /// submission rather than talking to RPC themselves; a caller who
    /// wants to keep that ownership simply never calls this endpoint and
    /// submits the XDR from `prepare_swap`/etc. on their own.
    pub async fn submit_transaction(&self, signed_xdr: &str) -> Result<SubmitResult, EngineError> {
        let sent = self.rpc.send_transaction(signed_xdr).await?;
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
            let got = self.rpc.get_transaction(&sent.hash).await?;
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

        Ok(SubmitResult {
            hash: sent.hash,
            send_status: sent.status,
            final_status: "NOT_FOUND".to_string(),
            result_xdr: None,
            polled_attempts: MAX_POLLS,
        })
    }

    async fn fetch_reserves(&self) -> Result<PoolReserves, EngineError> {
        let map = self.fetch_pool_instance_map().await?;

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

        // LP total supply is not part of the pool's own storage: it lives on
        // the SEP-41 LP token contract (see the pool's DataKey::LpToken). Read
        // it from the actual LP token contract instance rather than scanning
        // the pool for an "LpTotalSup" fragment that does not exist.
        let lp_token = xdr::address_to_string(&xdr::instance_address(&map, "LpToken")?);
        let lp_total_supply = self.read_lp_total_supply(&lp_token).await?;

        Ok(PoolReserves {
            reserve_0,
            reserve_1,
            token_0: self.pool.token_0.clone(),
            token_1: self.pool.token_1.clone(),
            lp_total_supply,
            timestamp_last,
        })
    }

    /// Reads and decodes the pool contract's typed instance storage map
    /// exactly once per call, emitting the source-ledger gauge.
    async fn fetch_pool_instance_map(&self) -> Result<xdr::ScMap, EngineError> {
        let key = xdr::contract_instance_ledger_key(self.contract_id())?;
        let entries = self.rpc.get_ledger_entries(vec![key]).await?;

        let entry = entries
            .first()
            .ok_or_else(|| EngineError::NotFound("contract instance not found".into()))?;
        if let Some(ledger) = entry.last_modified {
            crate::observability::gauge("nodus_quote_source_ledger", ledger as f64);
        }
        crate::observability::gauge("nodus_provider_divergence_ledgers", 0.0);
        xdr::decode_instance_storage(&entry.xdr)
    }
}

fn i128_to_u128(v: i128) -> Option<u128> {
    u128::try_from(v).ok()
}
