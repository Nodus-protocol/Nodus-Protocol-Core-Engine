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

#[derive(Debug, Deserialize)]
pub struct QuoteQuery {
    pub amount_in: u128,
    pub token_in: String,
}

#[derive(Debug, Deserialize)]
pub struct LpBalanceQuery {
    pub address: String,
}

pub async fn reserves(State(ctx): State<AppState>) -> Result<impl IntoResponse, EngineError> {
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;
    Ok(Json(pool.get_reserves().await?))
}

pub async fn quote(
    State(ctx): State<AppState>,
    Query(q): Query<QuoteQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;
    Ok(Json(pool.get_quote(q.amount_in, &q.token_in).await?))
}

pub async fn lp_balance(
    State(ctx): State<AppState>,
    Query(q): Query<LpBalanceQuery>,
) -> Result<impl IntoResponse, EngineError> {
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;
    let balance = pool.lp_balance(&q.address).await?;
    Ok(Json(serde_json::json!({ "address": q.address, "lp_balance": balance.to_string() })))
}

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
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;
    Ok(Json(pool.build_swap_params(&req.to, req.amount_0_out, req.amount_1_out, req.deadline)))
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
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;
    Ok(Json(pool.build_add_liquidity_params(
        &req.from, &req.to,
        req.amount_0_desired, req.amount_1_desired,
        req.amount_0_min, req.amount_1_min,
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
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;
    Ok(Json(pool.build_remove_liquidity_params(
        &req.from, &req.to,
        req.liquidity, req.amount_0_min, req.amount_1_min,
        req.deadline,
    )))
}

pub async fn pool_stats(State(ctx): State<AppState>) -> Result<impl IntoResponse, EngineError> {
    let pool = ctx.pool.as_ref().ok_or_else(|| {
        EngineError::Internal("pool contract not configured".into())
    })?;

    let reserves = pool.get_reserves().await?;

    let price_0_in_1 = if reserves.reserve_0 > 0 {
        reserves.reserve_1 as f64 / reserves.reserve_0 as f64
    } else { 0.0 };

    let price_1_in_0 = if reserves.reserve_1 > 0 {
        reserves.reserve_0 as f64 / reserves.reserve_1 as f64
    } else { 0.0 };

    let k = reserves.reserve_0
        .checked_mul(reserves.reserve_1)
        .unwrap_or(u128::MAX);

    Ok(Json(serde_json::json!({
        "reserves": reserves,
        "price_token0_in_token1": price_0_in_1,
        "price_token1_in_token0": price_1_in_0,
        "k_invariant": k.to_string(),
        "fee_bps": math::FEE_DENOMINATOR - math::FEE_NUMERATOR,
    })))
}

pub async fn not_configured() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "code": "POOL_NOT_CONFIGURED",
            "message": "POOL_CONTRACT_ID, SOROBAN_RPC_URL, POOL_TOKEN_0, POOL_TOKEN_1 must be set"
        })),
    )
}
