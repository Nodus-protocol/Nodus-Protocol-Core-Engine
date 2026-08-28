use std::collections::HashMap;
use std::env;

use crate::auth::{parse_service_keys, ServiceKey};

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub network: Network,
    #[allow(dead_code)]
    pub horizon_url: String,
    pub max_retry_attempts: u32,
    pub retry_initial_delay_ms: u64,
    #[allow(dead_code)]
    pub webhook_timeout_secs: u64,
    pub pool: Option<PoolConfig>,
    pub redis_url: Option<String>,
    pub idempotency_ttl_secs: u64,
    pub release: String,
    pub manifest: String,
    pub expected_manifest: String,
    pub contract_spec: String,
    pub supported_contract_spec: String,
    pub expected_network: Network,
    pub reconciliation_lag_seconds: u64,
    pub reconciliation_lag_max_seconds: u64,
    /// Signed-request service identities. Empty unless `ENGINE_AUTH_KEYS` is set.
    pub auth_keys: HashMap<String, ServiceKey>,
    /// Escape hatch for local development only — refuses to activate on mainnet.
    pub auth_disabled: bool,
    pub auth_clock_skew_secs: u64,
    pub auth_replay_window_secs: u64,
    /// Explicit CORS allow-list. Empty means CORS is disabled entirely
    /// (no browser origin may call the engine cross-origin).
    pub cors_allowed_origins: Vec<String>,
    /// Operator attestation that TLS is terminated in front of this engine
    /// (load balancer, service mesh mTLS, sidecar proxy, ...). The engine
    /// itself speaks plain HTTP; on mainnet it refuses to start unless this
    /// is set, so an operator can't accidentally expose the plaintext port.
    pub tls_terminated_upstream: bool,
    /// Explicit override to run without the TLS attestation above. Only
    /// meant for local development — `validate()` still blocks mainnet.
    pub allow_insecure_transport: bool,
}

// Config intentionally excludes secrets from its Debug output — never derive
// Debug here, since `auth_keys` holds raw HMAC secret bytes.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("network", &self.network)
            .field("pool_configured", &self.pool.is_some())
            .field("redis_configured", &self.redis_url.is_some())
            .field("release", &self.release)
            .field("expected_network", &self.expected_network)
            .field(
                "reconciliation_lag_seconds",
                &self.reconciliation_lag_seconds,
            )
            .field("auth_keys_configured", &self.auth_keys.len())
            .field("auth_disabled", &self.auth_disabled)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    /// Parses the `network` field callers declare on transaction
    /// preparation/validation/submission requests. Deliberately strict —
    /// no aliases, no case-folding — so a typo fails loudly as an
    /// `InvalidRequest` rather than silently matching the wrong network.
    pub fn parse(s: &str) -> Result<Self, crate::utils::EngineError> {
        match s {
            "mainnet" => Ok(Network::Mainnet),
            "testnet" => Ok(Network::Testnet),
            other => Err(crate::utils::EngineError::InvalidRequest(format!(
                "network must be 'mainnet' or 'testnet', got '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub soroban_rpc_url: String,
    pub contract_id: String,
    pub token_0: String,
    pub token_1: String,
    /// Classic (non-resource) base fee, in stroops, before Soroban RPC's
    /// simulated resource fee is added on top.
    pub base_fee_stroops: u32,
    /// Hard ceiling on the total prepared fee (base + resource), in
    /// stroops. Transaction preparation refuses to hand back a transaction
    /// priced above this, regardless of what simulation says it costs.
    pub fee_ceiling_stroops: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let network = match env::var("NETWORK").unwrap_or_default().as_str() {
            "mainnet" => Network::Mainnet,
            _ => Network::Testnet,
        };

        let horizon_url = env::var("HORIZON_URL").unwrap_or_else(|_| match network {
            Network::Mainnet => "https://horizon.stellar.org".into(),
            Network::Testnet => "https://horizon-testnet.stellar.org".into(),
        });

        let pool = {
            let rpc = env::var("SOROBAN_RPC_URL").ok();
            let contract = env::var("POOL_CONTRACT_ID").ok();
            let t0 = env::var("POOL_TOKEN_0").ok();
            let t1 = env::var("POOL_TOKEN_1").ok();

            match (rpc, contract, t0, t1) {
                (Some(rpc), Some(contract), Some(t0), Some(t1)) => Some(PoolConfig {
                    soroban_rpc_url: rpc,
                    contract_id: contract,
                    token_0: t0,
                    token_1: t1,
                    base_fee_stroops: env::var("POOL_BASE_FEE_STROOPS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(100),
                    fee_ceiling_stroops: env::var("POOL_FEE_CEILING_STROOPS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(10_000_000),
                }),
                _ => None,
            }
        };

        Self {
            port: env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            network,
            horizon_url,
            max_retry_attempts: env::var("MAX_RETRY_ATTEMPTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            retry_initial_delay_ms: env::var("RETRY_INITIAL_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500),
            webhook_timeout_secs: env::var("WEBHOOK_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            pool,
            redis_url: env::var("REDIS_URL").ok(),
            idempotency_ttl_secs: env::var("IDEMPOTENCY_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(86_400),
            release: env::var("RELEASE_VERSION").unwrap_or_else(|_| "unknown".into()),
            manifest: env::var("RELEASE_MANIFEST_SHA").unwrap_or_default(),
            expected_manifest: env::var("EXPECTED_MANIFEST_SHA").unwrap_or_default(),
            contract_spec: env::var("CONTRACT_SPEC_VERSION").unwrap_or_default(),
            supported_contract_spec: env::var("SUPPORTED_CONTRACT_SPEC_VERSION")
                .unwrap_or_default(),
            expected_network: match env::var("EXPECTED_NETWORK").unwrap_or_default().as_str() {
                "mainnet" => Network::Mainnet,
                _ => Network::Testnet,
            },
            reconciliation_lag_seconds: env::var("RECONCILIATION_LAG_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(u64::MAX),
            reconciliation_lag_max_seconds: env::var("RECONCILIATION_LAG_MAX_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            auth_keys: match env::var("ENGINE_AUTH_KEYS") {
                Ok(raw) if !raw.trim().is_empty() => parse_service_keys(&raw).unwrap_or_else(|e| {
                    panic!("ENGINE_AUTH_KEYS is invalid: {e}");
                }),
                _ => HashMap::new(),
            },
            auth_disabled: env::var("ENGINE_AUTH_DISABLED")
                .map(|v| v == "true")
                .unwrap_or(false),
            auth_clock_skew_secs: env::var("ENGINE_AUTH_CLOCK_SKEW_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            auth_replay_window_secs: env::var("ENGINE_AUTH_REPLAY_WINDOW_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            cors_allowed_origins: env::var("CORS_ALLOWED_ORIGINS")
                .ok()
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            tls_terminated_upstream: env::var("TLS_TERMINATED_UPSTREAM")
                .map(|v| v == "true")
                .unwrap_or(false),
            allow_insecure_transport: env::var("ENGINE_ALLOW_INSECURE_TRANSPORT")
                .map(|v| v == "true")
                .unwrap_or(false),
        }
    }

    /// Fails startup when the configuration would be unsafe to run in
    /// production: no service keys configured (or auth explicitly
    /// disabled) while targeting mainnet. Local/testnet development may
    /// still opt out via `ENGINE_AUTH_DISABLED=true`.
    pub fn validate(&self) {
        if self.network == Network::Mainnet {
            if self.auth_disabled {
                panic!(
                    "refusing to start: ENGINE_AUTH_DISABLED=true is not allowed when NETWORK=mainnet"
                );
            }
            if self.auth_keys.is_empty() {
                panic!(
                    "refusing to start: NETWORK=mainnet requires at least one key in ENGINE_AUTH_KEYS"
                );
            }
            if self.redis_url.is_none() {
                panic!(
                    "refusing to start: NETWORK=mainnet requires REDIS_URL for the durable \
                     idempotency store (no in-memory fallback is permitted)"
                );
            }
        }
        if !self.auth_disabled && self.auth_keys.is_empty() {
            tracing::warn!("ENGINE_AUTH_KEYS is empty — every non-health request will be rejected");
        }

        if !self.tls_terminated_upstream && !self.allow_insecure_transport {
            if self.network == Network::Mainnet {
                panic!(
                    "refusing to start: set TLS_TERMINATED_UPSTREAM=true once TLS is terminated \
                     in front of this engine (load balancer / mesh mTLS), or \
                     ENGINE_ALLOW_INSECURE_TRANSPORT=true to explicitly accept plaintext transport"
                );
            }
            tracing::warn!(
                "TLS_TERMINATED_UPSTREAM is not set — this engine speaks plain HTTP and must sit \
                 behind a TLS-terminating proxy in any shared or production environment"
            );
        }
    }

    pub fn static_readiness(&self) -> Vec<&'static str> {
        let mut failures = Vec::new();
        if self.network != self.expected_network {
            failures.push("wrong_network");
        }
        if self.manifest.is_empty() || self.manifest != self.expected_manifest {
            failures.push("wrong_manifest");
        }
        if self.contract_spec.is_empty() || self.contract_spec != self.supported_contract_spec {
            failures.push("unsupported_contract_spec");
        }
        if self.pool.is_none() {
            failures.push("contract_not_configured");
        }
        if self.reconciliation_lag_seconds > self.reconciliation_lag_max_seconds {
            failures.push("reconciliation_backlog");
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
            port: 8080,
            network: Network::Mainnet,
            horizon_url: String::new(),
            max_retry_attempts: 1,
            retry_initial_delay_ms: 1,
            webhook_timeout_secs: 1,
            pool: None,
            redis_url: None,
            idempotency_ttl_secs: 1,
            release: "v1".into(),
            manifest: "actual".into(),
            expected_manifest: "expected".into(),
            contract_spec: "2".into(),
            supported_contract_spec: "1".into(),
            expected_network: Network::Testnet,
            reconciliation_lag_seconds: 31,
            reconciliation_lag_max_seconds: 30,
            auth_keys: HashMap::new(),
            auth_disabled: true,
            auth_clock_skew_secs: 60,
            auth_replay_window_secs: 300,
            cors_allowed_origins: Vec::new(),
            tls_terminated_upstream: false,
            allow_insecure_transport: true,
        }
    }

    #[test]
    fn readiness_rejects_identity_and_contract_mismatches() {
        let config = base_config();
        assert_eq!(
            config.static_readiness(),
            vec![
                "wrong_network",
                "wrong_manifest",
                "unsupported_contract_spec",
                "contract_not_configured",
                "reconciliation_backlog",
            ]
        );
    }

    #[test]
    fn validate_panics_on_mainnet_without_auth_keys() {
        let mut config = base_config();
        config.auth_disabled = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| config.validate()));
        assert!(result.is_err());
    }

    #[test]
    fn validate_panics_on_mainnet_with_auth_disabled() {
        let mut config = base_config();
        config.auth_disabled = true;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| config.validate()));
        assert!(result.is_err());
    }

    #[test]
    fn validate_panics_on_mainnet_without_tls_attestation() {
        let mut config = base_config();
        config.auth_disabled = false;
        config.auth_keys.insert(
            "k".into(),
            crate::auth::parse_service_keys("k:00112233445566778899aabbccddeeff:read")
                .unwrap()
                .remove("k")
                .unwrap(),
        );
        config.allow_insecure_transport = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| config.validate()));
        assert!(result.is_err());
    }

    #[test]
    fn validate_passes_on_testnet_with_defaults() {
        let mut config = base_config();
        config.network = Network::Testnet;
        config.validate();
    }

    #[test]
    fn validate_panics_on_mainnet_without_redis() {
        let mut config = base_config();
        config.auth_disabled = false;
        config.auth_keys.insert(
            "k".into(),
            crate::auth::parse_service_keys("k:00112233445566778899aabbccddeeff:read")
                .unwrap()
                .remove("k")
                .unwrap(),
        );
        config.allow_insecure_transport = true;
        config.redis_url = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| config.validate()));
        assert!(result.is_err());
    }

    #[test]
    fn validate_passes_on_mainnet_with_redis_auth_and_tls() {
        let mut config = base_config();
        config.auth_disabled = false;
        config.auth_keys.insert(
            "k".into(),
            crate::auth::parse_service_keys("k:00112233445566778899aabbccddeeff:read")
                .unwrap()
                .remove("k")
                .unwrap(),
        );
        config.allow_insecure_transport = true;
        config.redis_url = Some("redis://localhost:6379".into());
        config.validate();
    }
}
