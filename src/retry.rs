use std::future::Future;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_delay_ms: u64,
    pub backoff_multiplier: f64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 500,
            backoff_multiplier: 2.0,
            max_delay_ms: 10_000,
        }
    }
}

impl RetryConfig {
    pub fn new(max_attempts: u32, initial_delay_ms: u64) -> Self {
        Self {
            max_attempts,
            initial_delay_ms,
            ..Default::default()
        }
    }
}

pub async fn retry<F, Fut, T, E>(config: &RetryConfig, mut f: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay_ms = config.initial_delay_ms;

    for attempt in 1..=config.max_attempts {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) if attempt == config.max_attempts => return Err(e),
            Err(e) => {
                tracing::warn!(attempt, max = config.max_attempts, delay_ms, error = %e, "retrying");
                sleep(Duration::from_millis(delay_ms)).await;
                delay_ms =
                    ((delay_ms as f64 * config.backoff_multiplier) as u64).min(config.max_delay_ms);
            }
        }
    }

    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn succeeds_on_first_attempt() {
        let config = RetryConfig::new(3, 1);
        let result: Result<i32, &str> = retry(&config, || async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn succeeds_on_third_attempt() {
        let config = RetryConfig::new(3, 1);
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<i32, &str> = retry(&config, || {
            let count = attempts_clone.clone();
            async move {
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err("not yet")
                } else {
                    Ok(99)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 99);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fails_after_max_attempts() {
        let config = RetryConfig::new(2, 1);
        let result: Result<i32, &str> = retry(&config, || async { Err("always fails") }).await;
        assert!(result.is_err());
    }
}
