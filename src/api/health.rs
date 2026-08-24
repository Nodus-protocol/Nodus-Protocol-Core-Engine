use crate::api::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "alive"})))
}

pub async fn readyz(State(ctx): State<AppState>) -> impl IntoResponse {
    let health = ctx.engine.health().await;
    let static_failures = ctx.config.static_readiness();
    let status = if health.status == "ready" && static_failures.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({"dependencies": health, "failures": static_failures})),
    )
}

pub async fn metrics() -> impl IntoResponse {
    crate::observability::encode("metrics")
}
