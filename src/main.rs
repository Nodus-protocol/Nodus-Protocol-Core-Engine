mod adapters;
mod api;
mod batch;
mod circuit_breaker;
mod config;
mod engine;
mod idempotency;
mod middleware;
mod pool;
mod rates;
mod retry;
mod router;
mod store;
mod utils;
mod validation;
mod webhook;

use std::sync::Arc;

use axum::{middleware as axum_middleware, routing::{delete, get, post, put}, Router};
use tokio::net::TcpListener;
use tower_http::{cors::{Any, CorsLayer}, trace::TraceLayer};

use adapters::StellarAdapter;
use api::{AppContext, AppState};
use circuit_breaker::CircuitBreaker;
use config::{Config, Network};
use engine::Engine;
use pool::{contract::ContractClient, soroban::SorobanRpc};
use rates::RateService;
use retry::RetryConfig;
use webhook::WebhookStore;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nodus_core_engine=info,tower_http=info".into()),
        )
        .init();

    let cfg = Config::from_env();

    let stellar_raw: Arc<dyn adapters::ChainAdapter> = match cfg.network {
        Network::Mainnet => {
            tracing::info!("network: Stellar Mainnet");
            Arc::new(StellarAdapter::mainnet())
        }
        Network::Testnet => {
            tracing::info!("network: Stellar Testnet");
            Arc::new(StellarAdapter::testnet())
        }
    };

    let stellar = Arc::new(CircuitBreaker::new(stellar_raw, 5, 30));
    let retry_config = RetryConfig::new(cfg.max_retry_attempts, cfg.retry_initial_delay_ms);
    let engine = Arc::new(Engine::new(vec![stellar], retry_config));
    let webhooks = Arc::new(WebhookStore::new());
    let rates = RateService::new();

    let pool_client = cfg.pool.as_ref().map(|p| {
        tracing::info!(
            contract = %p.contract_id,
            rpc = %p.soroban_rpc_url,
            "AMM pool contract configured"
        );
        ContractClient::new(
            SorobanRpc::new(&p.soroban_rpc_url),
            &p.contract_id,
            &p.token_0,
            &p.token_1,
        )
    });

    if pool_client.is_none() {
        tracing::warn!("SOROBAN_RPC_URL / POOL_CONTRACT_ID not set — pool endpoints will return 503");
    }

    let state: AppState = Arc::new(AppContext {
        engine,
        rates,
        webhooks,
        pool: pool_client,
    });

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);

    let app = Router::new()
        // Health
        .route("/healthz",                             get(api::health::healthz))
        // Payments
        .route("/api/v1/payments",                     post(api::payments::initiate).get(api::payments::list))
        .route("/api/v1/payments/simulate",            post(api::payments::simulate))
        .route("/api/v1/payments/batch",               post(api::batch::submit))
        .route("/api/v1/payments/:id",                 get(api::payments::get))
        .route("/api/v1/payments/:id/receipt",         get(api::payments::receipt))
        // Fees & Rates
        .route("/api/v1/fees/current",                 get(api::fees::current))
        .route("/api/v1/rates",                        get(api::rates::get))
        // AMM Pool — read
        .route("/api/v1/pool/reserves",                         get(api::pool::reserves))
        .route("/api/v1/pool/quote",                            get(api::pool::quote))
        .route("/api/v1/pool/reverse-quote",                    get(api::pool::reverse_quote))
        .route("/api/v1/pool/lp-balance",                       get(api::pool::lp_balance))
        .route("/api/v1/pool/stats",                            get(api::pool::pool_stats))
        // AMM Pool — simulation (pure math, no signing required)
        .route("/api/v1/pool/simulate/add-liquidity",           get(api::pool::simulate_add_liquidity))
        .route("/api/v1/pool/simulate/remove-liquidity",        get(api::pool::simulate_remove_liquidity))
        // AMM Pool — unsigned tx builders
        .route("/api/v1/pool/build/swap",                       post(api::pool::build_swap))
        .route("/api/v1/pool/build/add-liquidity",              post(api::pool::build_add_liquidity))
        .route("/api/v1/pool/build/remove-liquidity",           post(api::pool::build_remove_liquidity))
        // Webhooks
        .route("/api/v1/webhooks",                     post(api::webhooks::register).get(api::webhooks::list))
        .route("/api/v1/webhooks/:id",                 delete(api::webhooks::delete))
        .route("/api/v1/webhooks/:id/toggle",          put(api::webhooks::toggle))
        .layer(axum_middleware::from_fn(middleware::inject_request_id))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).await.expect("failed to bind");
    tracing::info!("Nodus Protocol Core Engine listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
