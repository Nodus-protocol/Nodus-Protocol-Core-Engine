use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::AppContext;
use crate::config::Network;
use crate::pool::math;
use crate::pool::prepare::PrepareParams;
use crate::utils::{AssetId, EngineError, RationalPrice};

type AppState = Arc<AppContext>;

// ── Query types ───────────────────────────────────────────────────────────────

/// Quote request. `token_in` is a JSON-encoded [`AssetId`] passed as a
/// query-string parameter (URL-encoded). Bare symbol strings are rejected.
#[derive(Debug, Deserialize)]
pub struct QuoteQuery {
    pub amount_in: u128,
    /// JSON-serialised AssetId, URL-encoded.
    pub token_in: String,
    pub slippage_bps: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ReverseQuoteQuery {
    pub amount_out: u128,
    /// JSON-serialised AssetId, URL-encoded.
    pub token_out: String,
}

#[derive(Debug, Deserialize)]
pub struct LpBalanceQuery {
    pub address: String,
}

#[derive(Debug, Deserialize)]
pub struct SimulateRemoveQuery {
    pub address: String,
}

#[derive(Debug, Deserialize)]
pub struct SimulateAddQuery {
    pub amount_0: u128,
    pub amount_1: u128,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn reserves(State(ctx): State<AppState>) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    Ok(Json(pool.get_reserves().await?))
}

pub async fn quote(
    State(ctx): State<AppState>,
    Query(q): Query<QuoteQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let token_in: AssetId = parse_asset_param(&q.token_in, "token_in")?;
    let pool = pool_or_err(&ctx)?;
    let pq = pool.get_quote(q.amount_in, &token_in).await?;

    let mut body = serde_json::json!({
        "amount_in":        pq.amount_in.to_string(),
        "amount_out":       pq.amount_out.to_string(),
        "token_in":         pq.token_in,
        "token_out":        pq.token_out,
        "fee_bps":          pq.fee_bps,
        "price_impact_bps": pq.price_impact_bps,
        "effective_price":  pq.effective_price,
    });

    if let Some(slippage_bps) = q.slippage_bps {
        let min_out = apply_slippage(pq.amount_out, slippage_bps);
        body["min_amount_out"] = serde_json::json!(min_out.to_string());
        body["slippage_bps"] = serde_json::json!(slippage_bps);
    }

    Ok(Json(body))
}

/// Reverse quote: given a desired output amount, return the required input.
/// Uses `get_amount_in` (exact-output swap pricing).
/// `effective_price` is the exact rational `amount_in / amount_out`.
pub async fn reverse_quote(
    State(ctx): State<AppState>,
    Query(q): Query<ReverseQuoteQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let token_out: AssetId = parse_asset_param(&q.token_out, "token_out")?;
    let pool = pool_or_err(&ctx)?;
    let reserves = pool.get_reserves().await?;

    let key_out = token_out.canonical_key();
    let key_0 = reserves.token_0.canonical_key();
    let key_1 = reserves.token_1.canonical_key();

    let (reserve_out, reserve_in, token_in) = if key_out == key_0 {
        (reserves.reserve_0, reserves.reserve_1, reserves.token_1.clone())
    } else if key_out == key_1 {
        (reserves.reserve_1, reserves.reserve_0, reserves.token_0.clone())
    } else {
        return Err(EngineError::InvalidRequest(format!(
            "asset '{}' (key: '{key_out}') is not in this pool",
            token_out.symbol
        )));
    };

    let amount_in = math::get_amount_in(q.amount_out, reserve_in, reserve_out)
        .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

    let price_impact = math::price_impact_bps(amount_in, reserve_in);
    let effective_price = if q.amount_out > 0 {
        RationalPrice::new(amount_in, q.amount_out)
    } else {
        RationalPrice::zero()
    };

    Ok(Json(serde_json::json!({
        "amount_in":        amount_in.to_string(),
        "amount_out":       q.amount_out.to_string(),
        "token_in":         token_in,
        "token_out":        token_out,
        "fee_bps":          (math::FEE_DENOMINATOR - math::FEE_NUMERATOR) * 10,
        "price_impact_bps": price_impact,
        "effective_price":  effective_price,
    })))
}

pub async fn lp_balance(
    State(ctx): State<AppState>,
    Query(q): Query<LpBalanceQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let balance = pool.lp_balance(&q.address).await?;
    Ok(Json(serde_json::json!({
        "address":    q.address,
        "lp_balance": balance.to_string(),
    })))
}

pub async fn simulate_remove_liquidity(
    State(ctx): State<AppState>,
    Query(q): Query<SimulateRemoveQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let (lp_balance, reserves) =
        tokio::try_join!(pool.lp_balance(&q.address), pool.get_reserves())?;

    if lp_balance == 0 || reserves.lp_total_supply == 0 {
        return Ok(Json(serde_json::json!({
            "address":           q.address,
            "lp_balance":        "0",
            "amount_0_redeemed": "0",
            "amount_1_redeemed": "0",
            "token_0":           reserves.token_0,
            "token_1":           reserves.token_1,
            "pool_share_bps":    0,
        })));
    }

    let (amt_0, amt_1) = math::withdrawal_amounts(
        lp_balance,
        reserves.reserve_0,
        reserves.reserve_1,
        reserves.lp_total_supply,
    )
    .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

    let share_bps = ((lp_balance * 10_000) / reserves.lp_total_supply) as u64;

    Ok(Json(serde_json::json!({
        "address":           q.address,
        "lp_balance":        lp_balance.to_string(),
        "amount_0_redeemed": amt_0.to_string(),
        "amount_1_redeemed": amt_1.to_string(),
        "token_0":           reserves.token_0,
        "token_1":           reserves.token_1,
        "pool_share_bps":    share_bps,
    })))
}

pub async fn simulate_add_liquidity(
    State(ctx): State<AppState>,
    Query(q): Query<SimulateAddQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let reserves = pool.get_reserves().await?;

    let lp_minted = math::lp_tokens_to_mint(
        q.amount_0, q.amount_1,
        reserves.reserve_0, reserves.reserve_1,
        reserves.lp_total_supply,
    )
    .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

    let optimal_amount_0 = (q.amount_1 * reserves.reserve_0)
        .checked_div(reserves.reserve_1)
        .unwrap_or(q.amount_0);
    let optimal_amount_1 = (q.amount_0 * reserves.reserve_1)
        .checked_div(reserves.reserve_0)
        .unwrap_or(q.amount_1);

    Ok(Json(serde_json::json!({
        "lp_tokens_minted":       lp_minted.to_string(),
        "amount_0_used":          optimal_amount_0.min(q.amount_0).to_string(),
        "amount_1_used":          optimal_amount_1.min(q.amount_1).to_string(),
        "token_0":                reserves.token_0,
        "token_1":                reserves.token_1,
        "lp_total_supply_before": reserves.lp_total_supply.to_string(),
    })))
}

/// Spot prices as exact rationals — no f64.
pub async fn pool_stats(State(ctx): State<AppState>) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let reserves = pool.get_reserves().await?;

    let price_0_in_1 = if reserves.reserve_0 > 0 {
        RationalPrice::new(reserves.reserve_1, reserves.reserve_0)
    } else {
        RationalPrice::zero()
    };
    let price_1_in_0 = if reserves.reserve_1 > 0 {
        RationalPrice::new(reserves.reserve_0, reserves.reserve_1)
    } else {
        RationalPrice::zero()
    };
    let k = reserves.reserve_0.saturating_mul(reserves.reserve_1);

    Ok(Json(serde_json::json!({
        "reserves":               reserves,
        "price_token0_in_token1": price_0_in_1,
        "price_token1_in_token0": price_1_in_0,
        "k_invariant":            k.to_string(),
        "fee_bps":                (math::FEE_DENOMINATOR - math::FEE_NUMERATOR) * 10,
    })))
}

// ── Prepare ───────────────────────────────────────────────────────────────────

fn parse_network(s: &str) -> Result<Network, EngineError> {
    Network::parse(s)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Deserialize)]
pub struct SwapParamsRequest {
    pub network: String,
    pub source_account: String,
    pub to: String,
    pub amount_0_out: u128,
    pub amount_1_out: u128,
    pub deadline: u64,
}

pub async fn build_swap(
    State(ctx): State<AppState>,
    Json(req): Json<SwapParamsRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let prepared = pool
        .prepare_swap(
            &req.to,
            req.amount_0_out,
            req.amount_1_out,
            PrepareParams {
                network: parse_network(&req.network)?,
                source_account: req.source_account,
                deadline: req.deadline,
            },
        )
        .await?;
    Ok(Json(prepared))
}

#[derive(Debug, Deserialize)]
pub struct AddLiquidityParamsRequest {
    pub network: String,
    pub from: String,
    pub to: String,
    pub amount_0_desired: u128,
    pub amount_1_desired: u128,
    pub amount_0_min: u128,
    pub amount_1_min: u128,
    pub deadline: u64,
}

pub async fn build_add_liquidity(
    State(ctx): State<AppState>,
    Json(req): Json<AddLiquidityParamsRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let network = parse_network(&req.network)?;
    let prepared = pool
        .prepare_add_liquidity(
            &req.from, &req.to,
            req.amount_0_desired, req.amount_1_desired,
            req.amount_0_min, req.amount_1_min,
            PrepareParams { network, source_account: req.from.clone(), deadline: req.deadline },
        )
        .await?;
    Ok(Json(prepared))
}

#[derive(Debug, Deserialize)]
pub struct RemoveLiquidityParamsRequest {
    pub network: String,
    pub from: String,
    pub to: String,
    pub liquidity: u128,
    pub amount_0_min: u128,
    pub amount_1_min: u128,
    pub deadline: u64,
}

pub async fn build_remove_liquidity(
    State(ctx): State<AppState>,
    Json(req): Json<RemoveLiquidityParamsRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let network = parse_network(&req.network)?;
    let prepared = pool
        .prepare_remove_liquidity(
            &req.from, &req.to,
            req.liquidity, req.amount_0_min, req.amount_1_min,
            PrepareParams { network, source_account: req.from.clone(), deadline: req.deadline },
        )
        .await?;
    Ok(Json(prepared))
}

// ── Validate ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ValidateRequest {
    pub xdr: String,
    pub network: String,
    pub source_account: String,
}

pub async fn validate(
    State(ctx): State<AppState>,
    Json(req): Json<ValidateRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let network = parse_network(&req.network)?;
    let review = pool
        .validate_transaction(&req.xdr, network, &req.source_account, unix_now())
        .await?;
    Ok(Json(review))
}

// ── Submit ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SubmitRequest {
    pub signed_xdr: String,
}

pub async fn submit(
    State(ctx): State<AppState>,
    Json(req): Json<SubmitRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let result = pool.submit_transaction(&req.signed_xdr).await?;
    Ok(Json(result))
}

#[allow(dead_code)]
pub async fn not_configured() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "code":    "POOL_NOT_CONFIGURED",
            "message": "POOL_CONTRACT_ID, SOROBAN_RPC_URL, POOL_TOKEN_0_JSON, POOL_TOKEN_1_JSON must be set",
        })),
    )
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn pool_or_err(ctx: &AppContext) -> Result<&crate::pool::ContractClient, EngineError> {
    ctx.pool
        .as_ref()
        .ok_or_else(|| EngineError::Internal("pool contract not configured".into()))
}

fn apply_slippage(amount: u128, slippage_bps: u64) -> u128 {
    amount.saturating_mul(10_000 - slippage_bps as u128) / 10_000
}

/// Parses a JSON-encoded [`AssetId`] from a query-string parameter value.
/// Returns a clear error if the value is not valid JSON or not a valid AssetId.
fn parse_asset_param(value: &str, param_name: &str) -> Result<AssetId, EngineError> {
    serde_json::from_str::<AssetId>(value).map_err(|e| {
        EngineError::InvalidRequest(format!(
            "'{param_name}' must be a JSON-encoded AssetId: {e}"
        ))
    })
}
