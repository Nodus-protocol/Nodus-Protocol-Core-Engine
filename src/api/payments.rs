use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::AppState;
use crate::utils::{ApiError, EngineError, Urgency};

impl IntoResponse for EngineError {
    fn into_response(self) -> Response {
        let status = match self.http_status() {
            404 => StatusCode::NOT_FOUND,
            400 => StatusCode::BAD_REQUEST,
            409 => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match &self {
            EngineError::NotFound(_) => "NOT_FOUND",
            EngineError::InvalidRequest(_) => "INVALID_REQUEST",
            EngineError::Conflict(_) => "CONFLICT",
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

#[derive(Debug, Deserialize, Serialize)]
pub struct InitiateRequest {
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    #[serde(default)]
    pub urgency: Urgency,
    pub idempotency_key: Option<String>,
}

pub async fn initiate(
    State(ctx): State<AppState>,
    Json(req): Json<InitiateRequest>,
) -> Result<(StatusCode, impl IntoResponse), EngineError> {
    let fingerprint = request_fingerprint(&req);
    if let Some(ref key) = req.idempotency_key {
        match ctx.engine.idempotency().get(key).await {
            Ok(Some(cached)) => {
                if cached["request_fingerprint"] != fingerprint {
                    crate::observability::counter("nodus_idempotency_conflicts_total");
                    return Err(EngineError::Conflict(
                        "idempotency key reused with a different request".into(),
                    ));
                }
                return Ok((
                    StatusCode::OK,
                    Json(cached["response"].clone()).into_response(),
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "idempotency get failed, proceeding as first request")
            }
            _ => {}
        }
    }

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

    if let Some(key) = req.idempotency_key {
        let body = serde_json::to_value(&payment).unwrap_or_default();
        let cached = serde_json::json!({"request_fingerprint": fingerprint, "response": body});
        if let Err(e) = ctx.engine.idempotency().set(key, cached).await {
            tracing::warn!(error = %e, "failed to store idempotency key");
        }
    }

    Ok((StatusCode::CREATED, Json(payment).into_response()))
}

fn request_fingerprint(request: &InitiateRequest) -> String {
    let bytes = serde_json::to_vec(request).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
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
    pub token: String,
    #[serde(default)]
    pub urgency: Urgency,
}

pub async fn simulate(
    State(ctx): State<AppState>,
    Json(req): Json<SimulateRequest>,
) -> Result<impl IntoResponse, EngineError> {
    let result = ctx
        .engine
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

#[derive(Serialize)]
pub struct Receipt {
    pub payment_id: String,
    pub tx_hash: String,
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
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
        token: payment.token,
        chain: "stellar".into(),
        confirmed_at: payment.updated_at,
    }))
}
