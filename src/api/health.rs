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

pub async fn reconciliation_status(State(ctx): State<AppState>) -> impl IntoResponse {
    match &ctx.reconciliation {
        Some(recon) => match recon.status().await {
            Ok(resp) => (
                StatusCode::OK,
                Json(serde_json::to_value(resp).unwrap_or_default()),
            ),
            Err(_) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "caught_up": false,
                    "lag": 999,
                    "lag_seconds": 999,
                    "last_indexed_ledger": 0,
                })),
            ),
        },
        None => (
            StatusCode::OK,
            Json(serde_json::json!({
                "caught_up": true,
                "lag": 0,
                "lag_seconds": 0,
                "last_indexed_ledger": 0,
            })),
        ),
    }
}
