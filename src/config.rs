use std::env;

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Network {
    Mainnet,
    Testnet,
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub soroban_rpc_url: String,
    pub contract_id: String,
    pub token_0: String,
    pub token_1: String,
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

    #[test]
    fn readiness_rejects_identity_and_contract_mismatches() {
        let config = Config {
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
        };
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
}
