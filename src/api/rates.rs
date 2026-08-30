use crate::api::AppState;
use crate::utils::AssetId;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct RatesQuery {
    /// Comma-separated list of JSON-encoded AssetId objects, URL-encoded.
    /// Example: `?assets=%7B%22network%22%3A%22mainnet%22%2C...%7D,...`
    pub assets: Option<String>,
}

pub async fn get(State(ctx): State<AppState>, Query(q): Query<RatesQuery>) -> impl IntoResponse {
    let raw = q.assets.unwrap_or_default();
    let assets: Vec<AssetId> = if raw.is_empty() {
        // Default: XLM native on the engine's configured network
        let net = match ctx.config.network {
            crate::config::Network::Mainnet => "mainnet",
            crate::config::Network::Testnet => "testnet",
        };
        vec![AssetId::native(net)]
    } else {
        // Each comma-separated token is a JSON-encoded AssetId
        let mut parsed = Vec::new();
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match serde_json::from_str::<AssetId>(part) {
                Ok(a) => parsed.push(a),
                Err(e) => {
                    return Json(serde_json::json!({
                        "code": "INVALID_REQUEST",
                        "message": format!("'assets' entry is not a valid AssetId JSON: {e}\nvalue: {part}")
                    }))
                    .into_response();
                }
            }
        }
        parsed
    };

    let refs: Vec<&AssetId> = assets.iter().collect();
    Json(ctx.rates.rates_for(&refs).await).into_response()
}
