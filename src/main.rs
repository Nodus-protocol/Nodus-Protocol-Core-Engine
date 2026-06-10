mod adapters;
mod api;
mod config;
mod engine;
mod rates;
mod retry;
mod router;
mod store;
mod utils;

use std::sync::Arc;

use axum::{routing::{get, post}, Router};
use tokio::net::TcpListener;
use tower_http::{cors::{Any, CorsLayer}, trace::TraceLayer};

use adapters::StellarAdapter;
use api::{AppContext, AppState};
use config::{Config, Network};
use engine::Engine;
use rates::RateService;
use retry::RetryConfig;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nodus_core_engine=info,tower_http=info".into()),
        )
        .init();

    let cfg = Config::from_env();

    let stellar: Arc<dyn adapters::ChainAdapter> = match cfg.network {
        Network::Mainnet => {
            tracing::info!("network: Stellar Mainnet");
            Arc::new(StellarAdapter::mainnet())
        }
        Network::Testnet => {
            tracing::info!("network: Stellar Testnet");
            Arc::new(StellarAdapter::testnet())
        }
    };

    let retry_config = RetryConfig::new(cfg.max_retry_attempts, cfg.retry_initial_delay_ms);
    let engine = Engine::new(vec![stellar], retry_config);
    let rates = RateService::new();

    let state: AppState = Arc::new(AppContext { engine, rates });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/healthz",                         get(api::healthz))
        .route("/api/v1/payments",                 post(api::initiate_payment).get(api::list_payments))
        .route("/api/v1/payments/simulate",        post(api::simulate_payment))
        .route("/api/v1/payments/:id",             get(api::get_payment))
        .route("/api/v1/payments/:id/receipt",     get(api::get_receipt))
        .route("/api/v1/fees/current",             get(api::current_fees))
        .route("/api/v1/rates",                    get(api::get_rates))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).await.expect("failed to bind");
    tracing::info!("Nodus Protocol Core Engine listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
