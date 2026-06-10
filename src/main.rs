mod adapters;
mod api;
mod engine;
mod utils;

use std::{env, sync::Arc};

use axum::{
    routing::{get, post},
    Router,
};
use tokio::net::TcpListener;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

use adapters::StellarAdapter;
use api::AppState;
use engine::Engine;

#[tokio::main]
async fn main() {
    // Initialise structured logging — set RUST_LOG=debug for verbose output.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nodus_core_engine=info,tower_http=info".into()),
        )
        .init();

    // Select network based on NETWORK env var (defaults to testnet for safety).
    let adapter: Arc<dyn adapters::ChainAdapter> = match env::var("NETWORK")
        .unwrap_or_else(|_| "testnet".into())
        .as_str()
    {
        "mainnet" => {
            tracing::info!("connecting to Stellar Mainnet (Horizon)");
            Arc::new(StellarAdapter::mainnet())
        }
        _ => {
            tracing::info!("connecting to Stellar Testnet (Horizon)");
            Arc::new(StellarAdapter::testnet())
        }
    };

    let state: AppState = Arc::new(Engine::new(adapter));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // Health
        .route("/healthz", get(api::healthz))
        // Payments
        .route("/api/v1/payments",          post(api::initiate_payment).get(api::list_payments))
        .route("/api/v1/payments/simulate", post(api::simulate_payment))
        .route("/api/v1/payments/:id",      get(api::get_payment))
        .route("/api/v1/payments/:id/receipt", get(api::get_receipt))
        // Fees
        .route("/api/v1/fees/current",      get(api::current_fees))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state);

    let port = env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await.expect("failed to bind");

    tracing::info!("Nodus Protocol Core Engine listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
