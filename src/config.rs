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
}

#[derive(Debug, Clone, PartialEq)]
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
        }
    }
}
