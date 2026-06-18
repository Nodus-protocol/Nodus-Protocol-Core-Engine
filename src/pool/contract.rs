use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::pool::{math, soroban::SorobanRpc};
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

#[derive(Debug, Clone, Serialize)]
pub struct UnsignedTx {
    pub contract_id: String,
    pub function: String,
    pub args: serde_json::Value,
    pub note: &'static str,
}

struct CachedReserves {
    data: PoolReserves,
    fetched_at: Instant,
}

pub struct ContractClient {
    rpc: SorobanRpc,
    contract_id: String,
    token_0: String,
    token_1: String,
    cache: RwLock<Option<CachedReserves>>,
}

impl ContractClient {
    pub fn new(rpc: SorobanRpc, contract_id: &str, token_0: &str, token_1: &str) -> Self {
        Self {
            rpc,
            contract_id: contract_id.to_string(),
            token_0: token_0.to_string(),
            token_1: token_1.to_string(),
            cache: RwLock::new(None),
        }
    }

    pub async fn get_reserves(&self) -> Result<PoolReserves, EngineError> {
        {
            let guard = self.cache.read().await;
            if let Some(ref c) = *guard {
                if c.fetched_at.elapsed() < CACHE_TTL {
                    return Ok(c.data.clone());
                }
            }
        }

        let reserves = self.fetch_reserves().await?;

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

    pub async fn lp_balance(&self, address: &str) -> Result<u128, EngineError> {
        let key_xdr = self.lp_balance_key_xdr(address)?;
        let entries = self.rpc.get_ledger_entries(vec![key_xdr]).await?;
        if entries.is_empty() {
            return Ok(0);
        }
        parse_i128_from_xdr(&entries[0].xdr)
    }

    // Returns unsigned transaction parameters for the client to sign and submit.
    pub fn build_swap_params(
        &self,
        to: &str,
        amount_0_out: u128,
        amount_1_out: u128,
        deadline: u64,
    ) -> UnsignedTx {
        UnsignedTx {
            contract_id: self.contract_id.clone(),
            function: "swap".into(),
            args: serde_json::json!({
                "to": to,
                "amount_0_out": amount_0_out.to_string(),
                "amount_1_out": amount_1_out.to_string(),
                "deadline": deadline
            }),
            note: "Sign this with your Stellar wallet and submit via Horizon POST /transactions",
        }
    }

    pub fn build_add_liquidity_params(
        &self,
        from: &str,
        to: &str,
        amount_0_desired: u128,
        amount_1_desired: u128,
        amount_0_min: u128,
        amount_1_min: u128,
        deadline: u64,
    ) -> UnsignedTx {
        UnsignedTx {
            contract_id: self.contract_id.clone(),
            function: "add_liquidity".into(),
            args: serde_json::json!({
                "from": from,
                "to": to,
                "amount_0_desired": amount_0_desired.to_string(),
                "amount_1_desired": amount_1_desired.to_string(),
                "amount_0_min": amount_0_min.to_string(),
                "amount_1_min": amount_1_min.to_string(),
                "deadline": deadline
            }),
            note: "Sign this with your Stellar wallet and submit via Horizon POST /transactions",
        }
    }

    pub fn build_remove_liquidity_params(
        &self,
        from: &str,
        to: &str,
        liquidity: u128,
        amount_0_min: u128,
        amount_1_min: u128,
        deadline: u64,
    ) -> UnsignedTx {
        UnsignedTx {
            contract_id: self.contract_id.clone(),
            function: "remove_liquidity".into(),
            args: serde_json::json!({
                "from": from,
                "to": to,
                "liquidity": liquidity.to_string(),
                "amount_0_min": amount_0_min.to_string(),
                "amount_1_min": amount_1_min.to_string(),
                "deadline": deadline
            }),
            note: "Sign this with your Stellar wallet and submit via Horizon POST /transactions",
        }
    }

    async fn fetch_reserves(&self) -> Result<PoolReserves, EngineError> {
        let key = self.instance_key_xdr()?;
        let entries = self.rpc.get_ledger_entries(vec![key]).await?;

        if entries.is_empty() {
            return Err(EngineError::NotFound("contract instance not found".into()));
        }

        parse_instance_storage(&entries[0].xdr, &self.token_0, &self.token_1)
    }

    fn instance_key_xdr(&self) -> Result<String, EngineError> {
        // XDR for: LedgerKey::ContractData { contract, key: ScVal::LedgerKeyContractInstance, durability: Persistent }
        // Encoded as binary: type(CONTRACT_DATA=2), contract(ScAddress::Contract + 32 bytes), key(SCV_LEDGER_KEY_CONTRACT_INSTANCE=18), durability(PERSISTENT=1)
        let contract_bytes = parse_contract_id(&self.contract_id)?;
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_be_bytes()); // LedgerKeyType::ContractData
        buf.extend_from_slice(&1u32.to_be_bytes()); // ScAddress::Contract
        buf.extend_from_slice(&contract_bytes); // 32-byte contract hash
        buf.extend_from_slice(&18u32.to_be_bytes()); // ScValType::LedgerKeyContractInstance
        buf.extend_from_slice(&1u32.to_be_bytes()); // ContractDataDurability::Persistent
        Ok(B64.encode(&buf))
    }

    fn lp_balance_key_xdr(&self, address: &str) -> Result<String, EngineError> {
        let contract_bytes = parse_contract_id(&self.contract_id)?;
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
        buf.extend(std::iter::repeat(0u8).take(4 - rem));
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
