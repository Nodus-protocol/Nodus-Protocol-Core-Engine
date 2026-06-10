use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::engine::Engine;
use crate::utils::{ApiError, EngineError, Urgency};

pub type AppState = Arc<Engine>;

// ── Error handling ────────────────────────────────────────────────────────────

impl IntoResponse for EngineError {
    fn into_response(self) -> Response {
        let status = match self.http_status() {
            404 => StatusCode::NOT_FOUND,
            400 => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match &self {
            EngineError::NotFound(_) => "NOT_FOUND",
            EngineError::InvalidRequest(_) => "INVALID_REQUEST",
            EngineError::AdapterError(_) => "ADAPTER_ERROR",
            EngineError::NetworkError(_) => "NETWORK_ERROR",
            EngineError::Internal(_) => "INTERNAL_ERROR",
        };
        (
            status,
            Json(ApiError {
                code,
                message: self.to_string(),
            }),
        )
            .into_response()
    }
}

// ── Health ────────────────────────────────────────────────────────────────────

pub async fn healthz(State(engine): State<AppState>) -> impl IntoResponse {
    let health = engine.health().await;
    let status = if health.chain_reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health))
}

// ── Payments ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InitiatePaymentRequest {
    pub sender: String,
    pub recipient: String,
    /// Amount in the token's base unit (stroops for XLM).
    pub amount: u64,
    pub token: String,
    #[serde(default)]
    pub urgency: Urgency,
}

pub async fn initiate_payment(
    State(engine): State<AppState>,
    Json(req): Json<InitiatePaymentRequest>,
) -> Result<(StatusCode, impl IntoResponse), EngineError> {
    let payment = engine
        .initiate(
            req.sender,
            req.recipient,
            req.amount,
            req.token,
            req.urgency,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(payment)))
}

pub async fn get_payment(
    State(engine): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, EngineError> {
    let payment = engine.get(&id)?;
    Ok(Json(payment))
}

pub async fn list_payments(State(engine): State<AppState>) -> impl IntoResponse {
    Json(engine.list())
}

// ── Simulation ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SimulateRequest {
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    #[serde(default)]
    pub urgency: Urgency,
}

pub async fn simulate_payment(
    State(engine): State<AppState>,
    Json(req): Json<SimulateRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let result = engine
        .simulate(
            req.sender,
            req.recipient,
            req.amount,
            req.token,
            req.urgency,
        )
        .await?;
    Ok(Json(result))
}

// ── Fees ──────────────────────────────────────────────────────────────────────

pub async fn current_fees(State(engine): State<AppState>) -> impl IntoResponse {
    Json(engine.current_fees().await)
}

// ── Receipt ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Receipt {
    pub payment_id: String,
    pub tx_hash: String,
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    pub chain: &'static str,
    pub confirmed_at: String,
}

pub async fn get_receipt(
    State(engine): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, EngineError> {
    let payment = engine.get(&id)?;
    let tx_hash = payment.tx_hash.ok_or_else(|| {
        EngineError::InvalidRequest(format!("payment {id} has not been confirmed yet"))
    })?;
    Ok(Json(Receipt {
        payment_id: payment.id,
        tx_hash,
        sender: payment.sender,
        recipient: payment.recipient,
        amount: payment.amount,
        token: payment.token,
        chain: "stellar",
        confirmed_at: payment.updated_at,
    }))
}
