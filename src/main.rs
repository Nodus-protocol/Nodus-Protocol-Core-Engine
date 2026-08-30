mod adapters;
mod api;
mod auth;
mod batch;
mod circuit_breaker;
mod config;
mod engine;
mod idempotency;
mod middleware;
mod nonce_store;
mod observability;
mod pool;
mod rate_limit;
mod rates;
mod retry;
mod router;
mod store;
mod utils;
mod validation;
mod webhook;

use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    http::{HeaderName, HeaderValue, Method},
    middleware as axum_middleware,
    routing::{delete, get, post, put},
    Router,
};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use std::time::Duration;

use adapters::StellarAdapter;
use api::{AppContext, AppState};
use auth::Scope;
use circuit_breaker::CircuitBreaker;
use config::{Config, Network};
use engine::Engine;
use pool::contract::ContractClient;
use pool::soroban::SorobanRpc;
use rates::RateService;
use retry::RetryConfig;
use webhook::WebhookStore;

/// Headers a signed engine request is allowed to carry.
const ALLOWED_HEADERS: &[&str] = &[
    "content-type",
    "x-request-id",
    "x-nodus-key-id",
    "x-nodus-timestamp",
    "x-nodus-nonce",
    "x-nodus-signature",
    "x-nodus-scope",
    "x-nodus-network",
];

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nodus_core_engine=info,tower_http=info".into()),
        )
        .init();

    let cfg = Config::from_env();
    cfg.validate();

    observability::init(observability::Identity {
        network: match cfg.network {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
        }
        .into(),
        provider: reqwest::Url::parse(&cfg.horizon_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into()),
        release: cfg.release.clone(),
        manifest: cfg.manifest.clone(),
        contract: cfg.contract_spec.clone(),
    });
    observability::gauge(
        "nodus_reconciliation_lag_seconds",
        cfg.reconciliation_lag_seconds as f64,
    );

    let stellar_raw: Arc<dyn adapters::ChainAdapter> = match cfg.network {
        Network::Mainnet => {
            tracing::info!("network: Stellar Mainnet");
            Arc::new(StellarAdapter::new(&cfg.horizon_url))
        }
        Network::Testnet => {
            tracing::info!("network: Stellar Testnet");
            Arc::new(StellarAdapter::new(&cfg.horizon_url))
        }
    };

    let stellar = Arc::new(CircuitBreaker::new(stellar_raw, 5, 30));
    let retry_config = RetryConfig::new(cfg.max_retry_attempts, cfg.retry_initial_delay_ms);
    let claims = idempotency::create_claim_store(cfg.redis_url.as_deref(), cfg.network)
        .await
        .expect("idempotency claim store unavailable");
    let engine = Arc::new(Engine::new(vec![stellar], retry_config, claims));
    let webhooks = Arc::new(WebhookStore::new());
    let rates = RateService::new();

    let pool_client = cfg.pool.as_ref().map(|p| {
        tracing::info!(
            contract = %p.contract_id,
            rpc = %p.soroban_rpc_url,
            "AMM pool contract configured"
        );
        ContractClient::new(SorobanRpc::new(&p.soroban_rpc_url), p, cfg.network)
    });

    if pool_client.is_none() {
        tracing::warn!(
            "SOROBAN_RPC_URL / POOL_CONTRACT_ID not set — pool endpoints will return 503"
        );
    }

    let state: AppState = Arc::new(AppContext {
        engine,
        rates,
        webhooks,
        pool: pool_client,
        config: cfg.clone(),
    });

    // ── Auth: replay-protected signed-request verification ────────────────
    let (nonces, _nonce_eviction_task) = nonce_store::create_nonce_store(
        cfg.redis_url.as_deref(),
        Duration::from_secs(cfg.auth_replay_window_secs),
    )
    .await;
    let auth_state: auth::AuthState = Arc::new(auth::AuthConfig {
        keys: cfg.auth_keys.clone(),
        network: cfg.network,
        clock_skew: Duration::from_secs(cfg.auth_clock_skew_secs),
        replay_window: Duration::from_secs(cfg.auth_replay_window_secs),
        nonces,
    });
    let rate_limiter: rate_limit::RateLimiterState = Arc::new(rate_limit::RateLimiter::new());
    let auth_disabled = cfg.auth_disabled;

    // ── CORS: explicit allow-list only. No origins configured means no
    // CorsLayer is attached at all, so browsers cannot make cross-origin
    // calls against this deployment (the intended default for an
    // internal-only backend-to-engine service). ─────────────────────────
    let cors = build_cors(&cfg.cors_allowed_origins);

    let mut app = Router::new()
        // Health/readiness/metrics — always open, outside signed-request
        // auth. /healthz is a trivial liveness probe; /readyz additionally
        // reports engine + static-readiness failures; /metrics exposes the
        // Prometheus-style counters observability::init wires up above.
        .route("/healthz", get(api::health::healthz))
        .route("/readyz", get(api::health::readyz))
        .route("/metrics", get(api::health::metrics))
        // Reads. `GET /api/v1/payments` (list) lives here; `POST` on the
        // same path (initiate) is a submission and is registered separately
        // below in the tx_submit group, so each method carries its own
        // scope requirement.
        .merge(scoped(
            Router::new()
                .route("/api/v1/payments", get(api::payments::list))
                .route("/api/v1/payments/:id", get(api::payments::get))
                .route("/api/v1/payments/:id/receipt", get(api::payments::receipt))
                .route("/api/v1/fees/current", get(api::fees::current))
                .route("/api/v1/rates", get(api::rates::get))
                .route("/api/v1/pool/reserves", get(api::pool::reserves))
                .route("/api/v1/pool/quote", get(api::pool::quote))
                .route("/api/v1/pool/reverse-quote", get(api::pool::reverse_quote))
                .route("/api/v1/pool/lp-balance", get(api::pool::lp_balance))
                .route("/api/v1/pool/stats", get(api::pool::pool_stats))
                .route(
                    "/api/v1/pool/simulate/add-liquidity",
                    get(api::pool::simulate_add_liquidity),
                )
                .route(
                    "/api/v1/pool/simulate/remove-liquidity",
                    get(api::pool::simulate_remove_liquidity),
                ),
            Scope::Read,
            &auth_state,
            &rate_limiter,
            auth_disabled,
            100,
        ))
        // Transaction construction — pure, unsigned tx building, simulation,
        // and decode/policy-check of an already-prepared transaction XDR.
        // No chain effect.
        .merge(scoped(
            Router::new()
                .route("/api/v1/payments/simulate", post(api::payments::simulate))
                .route("/api/v1/pool/build/swap", post(api::pool::build_swap))
                .route(
                    "/api/v1/pool/build/add-liquidity",
                    post(api::pool::build_add_liquidity),
                )
                .route(
                    "/api/v1/pool/build/remove-liquidity",
                    post(api::pool::build_remove_liquidity),
                )
                .route("/api/v1/pool/validate", post(api::pool::validate)),
            Scope::TxConstruct,
            &auth_state,
            &rate_limiter,
            auth_disabled,
            50,
        ))
        // Transaction submission — moves funds. Tightest concurrency ceiling.
        .merge(scoped(
            Router::new()
                .route("/api/v1/payments", post(api::payments::initiate))
                .route("/api/v1/payments/batch", post(api::batch::submit))
                .route("/api/v1/pool/submit", post(api::pool::submit)),
            Scope::TxSubmit,
            &auth_state,
            &rate_limiter,
            auth_disabled,
            20,
        ))
        // Administration — webhook subscriptions.
        .merge(scoped(
            Router::new()
                .route(
                    "/api/v1/webhooks",
                    post(api::webhooks::register).get(api::webhooks::list),
                )
                .route("/api/v1/webhooks/:id", delete(api::webhooks::delete))
                .route("/api/v1/webhooks/:id/toggle", put(api::webhooks::toggle)),
            Scope::Admin,
            &auth_state,
            &rate_limiter,
            auth_disabled,
            20,
        ))
        .layer(DefaultBodyLimit::max(auth::MAX_BODY_BYTES))
        .layer(axum_middleware::from_fn(middleware::inject_request_id))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    if let Some(cors) = cors {
        app = app.layer(cors);
    }

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).await.expect("failed to bind");
    tracing::info!("Nodus Protocol Core Engine listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}

/// Wraps a group of same-scope routes with rate limiting and (unless
/// explicitly disabled for local development) signed-request auth.
///
/// Layer order matters: `route_layer` calls stack outermost-last, so auth
/// is attached *after* rate limiting here, making auth the outer layer that
/// runs first — the rate limiter can then key its budget off the
/// authenticated workload identity auth attaches to the request.
fn scoped(
    router: Router<AppState>,
    scope: Scope,
    auth_state: &auth::AuthState,
    rate_limiter: &rate_limit::RateLimiterState,
    auth_disabled: bool,
    max_concurrent: usize,
) -> Router<AppState> {
    let router = router
        .layer(ConcurrencyLimitLayer::new(max_concurrent))
        .route_layer(axum_middleware::from_fn_with_state(
            (rate_limiter.clone(), scope),
            rate_limit::enforce,
        ));

    if auth_disabled {
        tracing::warn!(scope = %scope, "ENGINE_AUTH_DISABLED=true — skipping signed-request verification for this scope (local development only)");
        router
    } else {
        router.route_layer(axum_middleware::from_fn_with_state(
            (auth_state.clone(), scope),
            auth::require_scope,
        ))
    }
}

/// Builds an explicit CORS policy from a comma-separated origin allow-list.
/// Returns `None` (no CORS layer at all) when the list is empty, which is
/// the correct default for an internal, backend-to-engine deployment: with
/// no `Access-Control-Allow-Origin` header, browsers refuse the response
/// entirely rather than trusting a permissive wildcard.
fn build_cors(allowed_origins: &[String]) -> Option<CorsLayer> {
    if allowed_origins.is_empty() {
        tracing::info!(
            "CORS: disabled (no CORS_ALLOWED_ORIGINS configured) — internal-only deployment"
        );
        return None;
    }

    let origins: Vec<HeaderValue> = allowed_origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::error!(origin = %o, error = %e, "ignoring invalid CORS origin");
                None
            }
        })
        .collect();

    if origins.is_empty() {
        tracing::warn!("CORS: all configured origins were invalid — CORS remains disabled");
        return None;
    }

    tracing::info!(origins = ?allowed_origins, "CORS: restricted to explicit origin allow-list");

    let headers: Vec<HeaderName> = ALLOWED_HEADERS
        .iter()
        .copied()
        .map(HeaderName::from_static)
        .collect();

    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(vec![Method::GET, Method::POST, Method::PUT, Method::DELETE])
            .allow_headers(headers),
    )
}
