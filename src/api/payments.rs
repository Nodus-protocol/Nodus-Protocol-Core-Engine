use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::utils::{ApiError, AssetId, EngineError, Urgency};

impl IntoResponse for EngineError {
    fn into_response(self) -> Response {
        let status = match self.http_status() {
            404 => StatusCode::NOT_FOUND,
            400 => StatusCode::BAD_REQUEST,
            409 => StatusCode::CONFLICT,
            412 => StatusCode::PRECONDITION_FAILED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match &self {
            EngineError::NotFound(_) => "NOT_FOUND",
            EngineError::InvalidRequest(_) => "INVALID_REQUEST",
            EngineError::Conflict(_) => "CONFLICT",
            EngineError::AdapterError(_) => "ADAPTER_ERROR",
            EngineError::NetworkError(_) => "NETWORK_ERROR",
            EngineError::Internal(_) => "INTERNAL_ERROR",
            EngineError::PreconditionFailed(_) => "PRECONDITION_FAILED",
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

#[derive(Debug, Deserialize, Serialize)]
pub struct InitiateRequest {
    pub sender: String,
    pub recipient: String,
    /// Amount in integer base units of `asset`.
    pub amount: u64,
    /// Canonical asset identity. Replaces the old bare `token` string.
    pub asset: AssetId,
    #[serde(default)]
    pub urgency: Urgency,
    pub idempotency_key: Option<String>,
}

pub async fn initiate(
    State(ctx): State<AppState>,
    Json(req): Json<InitiateRequest>,
) -> Result<Response, EngineError> {
    // No idempotency key — run the work unconditionally.
    let Some(client_key) = req.idempotency_key.clone() else {
        let payment = ctx
            .engine
            .initiate(
                req.sender,
                req.recipient,
                req.amount,
                req.token,
                req.urgency,
            )
            .await?;
        return Ok((StatusCode::CREATED, Json(payment)).into_response());
    };

    let network = match ctx.config.network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    };
    let namespace = IdempotencyNamespace::new(network, "payments.initiate");
    let request_body = serde_json::to_value(&req).unwrap_or_default();

    let outcome = ctx
        .engine
        .initiate(
            req.sender,
            req.recipient,
            req.amount,
            req.asset,
            req.urgency,
        )
        .await?;

    Ok(match outcome {
        IdempotentInitiation::Executed(payment) => {
            (StatusCode::CREATED, Json(payment)).into_response()
        }
        IdempotentInitiation::Replayed(response) => {
            (StatusCode::OK, Json(response)).into_response()
        }
    })
}

pub async fn get(
    State(ctx): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, EngineError> {
    Ok(Json(ctx.engine.get(&id)?))
}

pub async fn list(State(ctx): State<AppState>) -> impl IntoResponse {
    Json(ctx.engine.list())
}

#[derive(Debug, Deserialize)]
pub struct SimulateRequest {
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub asset: AssetId,
    #[serde(default)]
    pub urgency: Urgency,
}

pub async fn simulate(
    State(ctx): State<AppState>,
    Json(req): Json<SimulateRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let result = ctx
        .engine
        .simulate(req.sender, req.recipient, req.amount, req.asset, req.urgency)
        .await?;
    Ok(Json(result))
}

#[derive(Serialize)]
pub struct Receipt {
    pub payment_id: String,
    pub tx_hash: String,
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    /// Full canonical asset identity.
    pub asset: AssetId,
    pub chain: String,
    pub confirmed_at: String,
}

pub async fn receipt(
    State(ctx): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, EngineError> {
    let payment = ctx.engine.get(&id)?;
    let tx_hash = payment
        .tx_hash
        .ok_or_else(|| EngineError::InvalidRequest(format!("payment {id} is not yet confirmed")))?;
    Ok(Json(Receipt {
        payment_id: payment.id,
        tx_hash,
        sender: payment.sender,
        recipient: payment.recipient,
        amount: payment.amount,
        asset: payment.asset,
        chain: "stellar".into(),
        confirmed_at: payment.updated_at,
    }))
}
