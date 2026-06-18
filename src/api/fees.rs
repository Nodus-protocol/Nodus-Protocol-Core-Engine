use crate::api::AppState;
use axum::{extract::State, response::IntoResponse, Json};

pub async fn current(State(ctx): State<AppState>) -> impl IntoResponse {
    Json(ctx.engine.current_fees().await)
}
