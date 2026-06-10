use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use crate::api::AppState;
use crate::batch::{process_batch, BatchItem};
use crate::utils::EngineError;

pub async fn submit(
    State(ctx): State<AppState>,
    Json(items): Json<Vec<BatchItem>>,
) -> Result<(StatusCode, impl IntoResponse), EngineError> {
    let result = process_batch(ctx.engine.clone(), items).await?;
    Ok((StatusCode::MULTI_STATUS, Json(result)))
}
