use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub network: Network,
    pub horizon_url: String,
    pub max_retry_attempts: u32,
    pub retry_initial_delay_ms: u64,
    pub webhook_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Network {
    Mainnet,
    Testnet,
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
        }
    }
}
