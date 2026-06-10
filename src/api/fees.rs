use axum::{extract::State, response::IntoResponse, Json};
use crate::api::AppState;

pub async fn current(State(ctx): State<AppState>) -> impl IntoResponse {
    Json(ctx.engine.current_fees().await)
}
