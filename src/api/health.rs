use crate::api::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

pub async fn healthz(State(ctx): State<AppState>) -> impl IntoResponse {
    let health = ctx.engine.health().await;
    let status = if health.status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health))
}
