use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::api::AppState;
use crate::utils::EngineError;
use crate::webhook::WebhookEvent;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub url: String,
    pub secret: String,
    pub events: Vec<WebhookEvent>,
}

pub async fn register(
    State(ctx): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    if req.url.is_empty() || req.secret.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"code": "INVALID_REQUEST", "message": "url and secret are required"})),
        ).into_response();
    }
    let hook = ctx.webhooks.register(req.url, req.secret, req.events);
    (StatusCode::CREATED, Json(hook)).into_response()
}

pub async fn list(State(ctx): State<AppState>) -> impl IntoResponse {
    Json(ctx.webhooks.list())
}

pub async fn delete(
    State(ctx): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, EngineError> {
    ctx.webhooks.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn toggle(
    State(ctx): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, EngineError> {
    let hook = ctx.webhooks.get(&id)?;
    ctx.webhooks.set_active(&id, !hook.active)?;
    Ok(Json(ctx.webhooks.get(&id)?))
}
