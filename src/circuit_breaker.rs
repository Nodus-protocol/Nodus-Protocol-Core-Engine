use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::adapters::ChainAdapter;
use crate::utils::{EngineError, FeeEstimate, Payment};

#[derive(Debug)]
enum State {
    Closed { failures: u32 },
    Open { opened_at: Instant },
    HalfOpen,
}

pub struct CircuitBreaker {
    inner: Arc<dyn ChainAdapter>,
    state: Mutex<State>,
    failure_threshold: u32,
    reset_timeout: Duration,
}

impl CircuitBreaker {
    pub fn new(
        adapter: Arc<dyn ChainAdapter>,
        failure_threshold: u32,
        reset_timeout_secs: u64,
    ) -> Self {
        Self {
            inner: adapter,
            state: Mutex::new(State::Closed { failures: 0 }),
            failure_threshold,
            reset_timeout: Duration::from_secs(reset_timeout_secs),
        }
    }

    #[allow(dead_code)]
    pub async fn circuit_status(&self) -> &'static str {
        match *self.state.lock().await {
            State::Closed { .. } => "closed",
            State::Open { .. } => "open",
            State::HalfOpen => "half_open",
        }
    }

    async fn check_and_transition(&self) -> Result<(), EngineError> {
        let mut state = self.state.lock().await;
        match *state {
            State::Open { opened_at } if opened_at.elapsed() >= self.reset_timeout => {
                *state = State::HalfOpen;
                tracing::info!(adapter = self.inner.name(), "circuit half-open, probing");
                Ok(())
            }
            State::Open { .. } => Err(EngineError::AdapterError(format!(
                "{} circuit is open — requests blocked",
                self.inner.name()
            ))),
            _ => Ok(()),
        }
    }

    async fn on_success(&self) {
        let mut state = self.state.lock().await;
        if !matches!(*state, State::Closed { failures: 0 }) {
            tracing::info!(adapter = self.inner.name(), "circuit closed");
        }
        *state = State::Closed { failures: 0 };
    }

    async fn on_failure(&self) {
        let mut state = self.state.lock().await;
        match *state {
            State::Closed { failures } => {
                let next = failures + 1;
                if next >= self.failure_threshold {
                    tracing::warn!(
                        adapter = self.inner.name(),
                        failures = next,
                        "circuit opened"
                    );
                    *state = State::Open {
                        opened_at: Instant::now(),
                    };
                } else {
                    *state = State::Closed { failures: next };
                }
            }
            State::HalfOpen => {
                tracing::warn!(
                    adapter = self.inner.name(),
                    "probe failed, circuit re-opened"
                );
                *state = State::Open {
                    opened_at: Instant::now(),
                };
            }
            State::Open { .. } => {}
        }
    }
}

#[async_trait]
impl ChainAdapter for CircuitBreaker {
    async fn submit(&self, payment: &Payment) -> Result<String, EngineError> {
        self.check_and_transition().await?;
        match self.inner.submit(payment).await {
            Ok(hash) => {
                self.on_success().await;
                Ok(hash)
            }
            Err(e) => {
                self.on_failure().await;
                Err(e)
            }
        }
    }

    async fn fee_estimate(&self) -> Result<FeeEstimate, EngineError> {
        self.check_and_transition().await?;
        match self.inner.fee_estimate().await {
            Ok(f) => {
                self.on_success().await;
                Ok(f)
            }
            Err(e) => {
                self.on_failure().await;
                Err(e)
            }
        }
    }

    async fn is_confirmed(&self, tx_hash: &str) -> Result<bool, EngineError> {
        self.inner.is_confirmed(tx_hash).await
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}
