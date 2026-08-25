use base64::{engine::general_purpose::STANDARD as B64, Engine};
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

    /// **Known limitation:** this queries an `"LpBalance"` key on the pool
    /// contract's own instance storage. In the currently deployed contract
    /// (see `contracts/pool/src/lib.rs` /
    /// `contracts/pool/src/storage.rs::DataKey` in
    /// `Nodus-protocol/Nodus-Protocol-Smart-Contract`), LP balances and
    /// total supply live on a separate SEP-41 LP token contract
    /// (`DataKey::LpToken`), not in the pool's own storage — that key does
    /// not exist here, so this always resolves to `0`. Left as pre-existing
    /// behavior; fixing it requires querying the LP token contract address
    /// (tracked separately from this transaction-preparation work).
    pub async fn lp_balance(&self, address: &str) -> Result<u128, EngineError> {
        let key_xdr = self.lp_balance_key_xdr(address)?;
        let entries = self.rpc.get_ledger_entries(vec![key_xdr]).await?;
        if entries.is_empty() {
            return Ok(0);
        }
        parse_i128_from_xdr(&entries[0].xdr)
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
        let key = xdr::contract_instance_ledger_key(self.contract_id())?;
        let entries = self.rpc.get_ledger_entries(vec![key]).await?;

        if entries.is_empty() {
            return Err(EngineError::NotFound("contract instance not found".into()));
        }

        if let Some(ledger) = entries[0].last_modified {
            crate::observability::gauge("nodus_quote_source_ledger", ledger as f64);
        }
        crate::observability::gauge("nodus_provider_divergence_ledgers", 0.0);
        parse_instance_storage(&entries[0].xdr, &self.pool.token_0, &self.pool.token_1)
    }

    fn lp_balance_key_xdr(&self, address: &str) -> Result<String, EngineError> {
        let contract_bytes = parse_contract_id(self.contract_id())?;
        let addr_bytes = parse_contract_id(address)?;
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_be_bytes()); // CONTRACT_DATA
        buf.extend_from_slice(&1u32.to_be_bytes()); // ScAddress::Contract
        buf.extend_from_slice(&contract_bytes);
        // key: SCVec [ SCSymbol("LpBalance"), ScAddress::Contract(address) ]
        buf.extend_from_slice(&11u32.to_be_bytes()); // SCV_VEC
        buf.extend_from_slice(&2u32.to_be_bytes()); // vec length = 2
        buf.extend_from_slice(&7u32.to_be_bytes()); // SCV_SYMBOL
        let sym = b"LpBalance";
        buf.extend_from_slice(&(sym.len() as u32).to_be_bytes());
        buf.extend_from_slice(sym);
        pad4(&mut buf, sym.len());
        buf.extend_from_slice(&6u32.to_be_bytes()); // SCV_ADDRESS
        buf.extend_from_slice(&1u32.to_be_bytes()); // ScAddress::Contract
        buf.extend_from_slice(&addr_bytes);
        buf.extend_from_slice(&1u32.to_be_bytes()); // Persistent
        Ok(B64.encode(&buf))
    }
}

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

fn pad4(buf: &mut Vec<u8>, len: usize) {
    let rem = len % 4;
    if rem != 0 {
        buf.extend(std::iter::repeat_n(0u8, 4 - rem));
    }
}

fn parse_i128_from_xdr(xdr: &str) -> Result<u128, EngineError> {
    let bytes = B64
        .decode(xdr)
        .map_err(|e| EngineError::Internal(format!("decode xdr: {e}")))?;
    // ScVal::I128 is encoded as: type(SCV_I128=8), hi(i64 BE), lo(u64 BE)
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

fn parse_instance_storage(
    xdr: &str,
    token_0: &str,
    token_1: &str,
) -> Result<PoolReserves, EngineError> {
    // Minimal parsing: extract Reserve0 and Reserve1 i128 values from the instance XDR.
    // Full XDR parsing requires stellar-xdr crate; this is a structural approximation.
    // Contributors should replace with stellar-xdr deserialization for production.
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
        token_0: token_0.to_string(),
        token_1: token_1.to_string(),
        lp_total_supply: lp_supply,
        timestamp_last: ts,
    })
}

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

fn extract_u64_by_key(buf: &[u8], key: &[u8]) -> Option<u64> {
    let pos = buf.windows(key.len()).position(|w| w == key)?;
    let start = pos + key.len();
    if start + 8 > buf.len() {
        return None;
    }
    Some(u64::from_be_bytes(buf[start..start + 8].try_into().ok()?))
}
