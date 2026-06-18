use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::api::AppContext;
use crate::pool::math;
use crate::utils::EngineError;

type AppState = Arc<AppContext>;

// ── Query types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct QuoteQuery {
    pub amount_in: u128,
    pub token_in: String,
    /// Optional slippage tolerance in bps (e.g. 50 = 0.5%). When provided,
    /// `min_amount_out` is returned alongside the expected output.
    pub slippage_bps: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ReverseQuoteQuery {
    pub amount_out: u128,
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

// ── Existing handlers ─────────────────────────────────────────────────────────

pub async fn reserves(State(ctx): State<AppState>) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    Ok(Json(pool.get_reserves().await?))
}

pub async fn quote(
    State(ctx): State<AppState>,
    Query(q): Query<QuoteQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let pq = pool.get_quote(q.amount_in, &q.token_in).await?;

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
pub async fn reverse_quote(
    State(ctx): State<AppState>,
    Query(q): Query<ReverseQuoteQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let reserves = pool.get_reserves().await?;

    let (reserve_out, reserve_in, token_in) = if q.token_out == reserves.token_0 {
        (
            reserves.reserve_0,
            reserves.reserve_1,
            reserves.token_1.clone(),
        )
    } else if q.token_out == reserves.token_1 {
        (
            reserves.reserve_1,
            reserves.reserve_0,
            reserves.token_0.clone(),
        )
    } else {
        return Err(EngineError::InvalidRequest(format!(
            "token '{}' is not in this pool",
            q.token_out
        )));
    };

    let amount_in = math::get_amount_in(q.amount_out, reserve_in, reserve_out)
        .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

    let price_impact = math::price_impact_bps(amount_in, reserve_in);
    let effective_price = if q.amount_out > 0 {
        amount_in as f64 / q.amount_out as f64
    } else {
        0.0
    };

    Ok(Json(serde_json::json!({
        "amount_in":        amount_in.to_string(),
        "amount_out":       q.amount_out.to_string(),
        "token_in":         token_in,
        "token_out":        q.token_out,
        "fee_bps":          math::FEE_DENOMINATOR - math::FEE_NUMERATOR,
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

/// Simulate remove-liquidity for an address: fetches their LP balance then
/// computes the XLM/USDC they would receive on full withdrawal.
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

/// Simulate add-liquidity: given desired token amounts, compute estimated LP
/// tokens to be minted using current reserves.
pub async fn simulate_add_liquidity(
    State(ctx): State<AppState>,
    Query(q): Query<SimulateAddQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let reserves = pool.get_reserves().await?;

    let lp_minted = math::lp_tokens_to_mint(
        q.amount_0,
        q.amount_1,
        reserves.reserve_0,
        reserves.reserve_1,
        reserves.lp_total_supply,
    )
    .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;

    // Optimal amounts respect the current ratio; the smaller side dictates LP minted.
    let optimal_amount_0 = (q.amount_1 * reserves.reserve_0)
        .checked_div(reserves.reserve_1)
        .unwrap_or(q.amount_0);
    let optimal_amount_1 = (q.amount_0 * reserves.reserve_1)
        .checked_div(reserves.reserve_0)
        .unwrap_or(q.amount_1);

    Ok(Json(serde_json::json!({
        "lp_tokens_minted":   lp_minted.to_string(),
        "amount_0_used":      optimal_amount_0.min(q.amount_0).to_string(),
        "amount_1_used":      optimal_amount_1.min(q.amount_1).to_string(),
        "token_0":            reserves.token_0,
        "token_1":            reserves.token_1,
        "lp_total_supply_before": reserves.lp_total_supply.to_string(),
    })))
}

pub async fn pool_stats(State(ctx): State<AppState>) -> Result<impl IntoResponse, EngineError> {
    let pool = pool_or_err(&ctx)?;
    let reserves = pool.get_reserves().await?;

    let price_0_in_1 = if reserves.reserve_0 > 0 {
        reserves.reserve_1 as f64 / reserves.reserve_0 as f64
    } else {
        0.0
    };
    let price_1_in_0 = if reserves.reserve_1 > 0 {
        reserves.reserve_0 as f64 / reserves.reserve_1 as f64
    } else {
        0.0
    };
    let k = reserves.reserve_0.saturating_mul(reserves.reserve_1);

    Ok(Json(serde_json::json!({
        "reserves":              reserves,
        "price_token0_in_token1": price_0_in_1,
        "price_token1_in_token0": price_1_in_0,
        "k_invariant":           k.to_string(),
        "fee_bps":               math::FEE_DENOMINATOR - math::FEE_NUMERATOR,
    })))
}

// ── Build (unsigned tx) ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SwapParamsRequest {
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
    Ok(Json(pool.build_swap_params(
        &req.to,
        req.amount_0_out,
        req.amount_1_out,
        req.deadline,
    )))
}

#[derive(Debug, Deserialize)]
pub struct AddLiquidityParamsRequest {
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
    Ok(Json(pool.build_add_liquidity_params(
        &req.from,
        &req.to,
        req.amount_0_desired,
        req.amount_1_desired,
        req.amount_0_min,
        req.amount_1_min,
        req.deadline,
    )))
}

#[derive(Debug, Deserialize)]
pub struct RemoveLiquidityParamsRequest {
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
    Ok(Json(pool.build_remove_liquidity_params(
        &req.from,
        &req.to,
        req.liquidity,
        req.amount_0_min,
        req.amount_1_min,
        req.deadline,
    )))
}

#[allow(dead_code)]
pub async fn not_configured() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "code":    "POOL_NOT_CONFIGURED",
            "message": "POOL_CONTRACT_ID, SOROBAN_RPC_URL, POOL_TOKEN_0, POOL_TOKEN_1 must be set",
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
