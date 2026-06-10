use axum::{extract::{Query, State}, response::IntoResponse, Json};
use serde::Deserialize;
use crate::api::AppState;

#[derive(Debug, Deserialize)]
pub struct RatesQuery {
    pub tokens: Option<String>,
}

pub async fn get(
    State(ctx): State<AppState>,
    Query(q): Query<RatesQuery>,
) -> impl IntoResponse {
    let tokens: Vec<&str> = q
        .tokens
        .as_deref()
        .unwrap_or("XLM,USDC")
        .split(',')
        .map(str::trim)
        .collect();
    Json(ctx.rates.rates_for(&tokens).await)
}
