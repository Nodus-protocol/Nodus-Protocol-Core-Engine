use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

use crate::adapters::ChainAdapter;
use crate::idempotency::{
    request_fingerprint, ClaimOutcome, ClaimStore, ClaimToken, IdempotencyNamespace,
};
use crate::retry::{retry, RetryConfig};
use crate::router::Router;
use crate::store::PaymentStore;
use crate::utils::{now_utc, EngineError, Payment, PaymentStatus, Urgency};
use crate::validation;

/// How long a worker may hold an in-flight idempotency claim before another
/// worker is allowed to take over an `InFlight` (no-side-effect) claim.
const CLAIM_LEASE: Duration = Duration::from_secs(30);

pub struct Engine {
    router: Router,
    store: PaymentStore,
    claims: Arc<dyn ClaimStore>,
    retry_config: RetryConfig,
}

impl Engine {
    pub fn new(
        adapters: Vec<Arc<dyn ChainAdapter>>,
        retry_config: RetryConfig,
        claims: Arc<dyn ClaimStore>,
    ) -> Self {
        Self {
            router: Router::new(adapters),
            store: PaymentStore::new(),
            claims,
            retry_config,
        }
    }

    /// Initiate a payment. This is the non-idempotent path: every call runs
    /// the work. Callers that carry a client idempotency key must go through
    /// [`Engine::initiate_idempotent`] instead.
    pub async fn initiate(
        &self,
        sender: String,
        recipient: String,
        amount: u64,
        token: String,
        urgency: Urgency,
    ) -> Result<Payment, EngineError> {
        self.run_initiation(sender, recipient, amount, token, urgency, None)
            .await
    }

    /// Initiate a payment under an atomic idempotency claim.
    ///
    /// A single `claim` both reserves the in-flight slot and returns any
    /// existing disposition, so two concurrent identical requests never both
    /// execute. Before the irreversible submission the claim is advanced to
    /// `Submitting` with the payment id as the execution reference, so a
    /// crash between submit and result cannot be turned into a second
    /// submission by a takeover — the successor is told to reconcile.
    pub async fn initiate_idempotent(
        &self,
        req: IdempotentRequest<'_>,
    ) -> Result<IdempotentInitiation, EngineError> {
        let key = req.namespace.key(req.client_key);
        let fingerprint = request_fingerprint(req.request_body);
        let owner = Uuid::new_v4().to_string();

        match self
            .claims
            .claim(&key, &fingerprint, &owner, CLAIM_LEASE, req.ttl)
            .await?
        {
            ClaimOutcome::Replay { response } => Ok(IdempotentInitiation::Replayed(response)),
            ClaimOutcome::InFlight => Err(EngineError::Conflict(
                "a request with this idempotency key is already in progress".into(),
            )),
            ClaimOutcome::AwaitingResult { execution_ref } => {
                // A prior attempt submitted but never recorded a result. If
                // that submission is a payment we know locally and it has
                // reached a terminal state, replay it; otherwise the outcome
                // is genuinely unknown and the caller must retry later. Under
                // no circumstance do we submit again under this key.
                match self.store.get(&execution_ref) {
                    Ok(p) if is_terminal(&p.status) => Ok(IdempotentInitiation::Replayed(
                        serde_json::to_value(&p).unwrap_or_default(),
                    )),
                    _ => Err(EngineError::PreconditionFailed(
                        "a submission for this idempotency key is in progress; retry shortly"
                            .into(),
                    )),
                }
            }
            ClaimOutcome::Claimed(claim_token) | ClaimOutcome::TookOver(claim_token) => {
                let checkpoint = SubmitCheckpoint {
                    store: &*self.claims,
                    token: &claim_token,
                    ttl: req.ttl,
                };
                let payment = self
                    .run_initiation(
                        req.sender,
                        req.recipient,
                        req.amount,
                        req.token,
                        req.urgency,
                        Some(&checkpoint),
                    )
                    .await?;
                let body = serde_json::to_value(&payment).unwrap_or_default();
                self.claims
                    .complete(&claim_token, &body, payment.tx_hash.as_deref(), req.ttl)
                    .await?;
                Ok(IdempotentInitiation::Executed(payment))
            }
        }
    }

    async fn run_initiation(
        &self,
        sender: String,
        recipient: String,
        amount: u64,
        token: String,
        urgency: Urgency,
        checkpoint: Option<&SubmitCheckpoint<'_>>,
    ) -> Result<Payment, EngineError> {
        validation::stellar_address(&sender)?;
        validation::stellar_address(&recipient)?;
        validation::amount(amount)?;
        validation::token(&token)?;

        let route = match self.router.select(&urgency).await {
            Ok(r) => r,
            Err(e) => {
                let now = now_utc();
                let payment = Payment {
                    id: Uuid::new_v4().to_string(),
                    sender,
                    recipient,
                    amount,
                    token,
                    status: PaymentStatus::Failed,
                    tx_hash: None,
                    fee_stroops: 0,
                    urgency,
                    error: Some(e.to_string()),
                    created_at: now.clone(),
                    updated_at: now,
                };
                self.store.insert(payment.clone());
                return Ok(payment);
            }
        };
        let now = now_utc();

        let payment = Payment {
            id: Uuid::new_v4().to_string(),
            sender,
            recipient,
            amount,
            token,
            status: PaymentStatus::Pending,
            tx_hash: None,
            fee_stroops: route.fee_stroops,
            urgency,
            error: None,
            created_at: now.clone(),
            updated_at: now,
        };

        self.store.insert(payment.clone());
        self.store
            .set_status(&payment.id, PaymentStatus::Processing)?;

        tracing::info!(
            payment_id = %payment.id,
            chain = route.adapter.name(),
            "payment processing"
        );

        // Record the execution reference durably *before* the irreversible
        // submission. A takeover after this point sees `Submitting` and
        // reconciles rather than re-submitting.
        if let Some(cp) = checkpoint {
            cp.record_submitting(&payment.id).await?;
        }

        let adapter = route.adapter.clone();
        let payment_snapshot = payment.clone();
        let cfg = self.retry_config.clone();

        let submitted_at = Instant::now();
        match retry(&cfg, || adapter.submit(&payment_snapshot)).await {
            Ok(tx_hash) => {
                crate::observability::counter("nodus_submission_accepted_total");
                crate::observability::gauge(
                    "nodus_inclusion_latency_seconds",
                    submitted_at.elapsed().as_secs_f64(),
                );
                self.store.set_confirmed(&payment.id, tx_hash.clone())?;
                tracing::info!(payment_id = %payment.id, %tx_hash, "confirmed");
            }
            Err(e) => {
                crate::observability::counter("nodus_submission_failed_total");
                self.store.set_failed(&payment.id, e.to_string())?;
                tracing::warn!(payment_id = %payment.id, error = %e, "failed");
            }
        }

        self.store.get(&payment.id)
    }

    pub fn get(&self, id: &str) -> Result<Payment, EngineError> {
        self.store.get(id)
    }

    pub fn list(&self) -> Vec<Payment> {
        self.store.list()
    }

    pub async fn simulate(
        &self,
        sender: String,
        recipient: String,
        amount: u64,
        token: String,
        urgency: Urgency,
    ) -> Result<SimulationResult, EngineError> {
        validation::amount(amount)?;
        let route = self.router.select(&urgency).await?;

        crate::observability::gauge(
            "nodus_simulation_resource_fee_stroops",
            route.fee_stroops as f64,
        );
        Ok(SimulationResult {
            sender,
            recipient,
            amount,
            token,
            fee_stroops: route.fee_stroops,
            chain: route.adapter.name().to_string(),
            estimated_confirmation_seconds: route.estimated_seconds,
        })
    }

    pub async fn current_fees(&self) -> Vec<crate::router::ChainFees> {
        self.router.all_fees().await
    }

    pub async fn health(&self) -> HealthStatus {
        let fees = self.router.all_fees().await;
        let any_up = fees.iter().any(|f| f.available);
        let durable_store = self.claims.ready().await;
        HealthStatus {
            status: if any_up && durable_store {
                "ready"
            } else {
                "not_ready"
            },
            chains: fees.iter().map(|f| f.chain).collect(),
            payments_in_store: self.store.len(),
            provider_ready: any_up,
            durable_store_ready: durable_store,
        }
    }
}

fn is_terminal(status: &PaymentStatus) -> bool {
    matches!(status, PaymentStatus::Confirmed | PaymentStatus::Failed)
}

/// Threaded into [`Engine::run_initiation`] so the durable claim can be
/// advanced to `Submitting` immediately before the chain submission.
struct SubmitCheckpoint<'a> {
    store: &'a dyn ClaimStore,
    token: &'a ClaimToken,
    ttl: Duration,
}

impl SubmitCheckpoint<'_> {
    async fn record_submitting(&self, execution_ref: &str) -> Result<(), EngineError> {
        self.store
            .mark_submitting(self.token, execution_ref, CLAIM_LEASE, self.ttl)
            .await
    }
}

/// Everything [`Engine::initiate_idempotent`] needs: the key namespace and
/// client key, the request body to fingerprint, the record TTL, and the
/// payment parameters.
pub struct IdempotentRequest<'a> {
    pub namespace: &'a IdempotencyNamespace,
    pub client_key: &'a str,
    pub request_body: &'a Value,
    pub ttl: Duration,
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    pub urgency: Urgency,
}

/// Result of an idempotent initiation: either the work ran now, or a
/// previously-recorded response was replayed.
pub enum IdempotentInitiation {
    Executed(Payment),
    Replayed(Value),
}

#[derive(serde::Serialize)]
pub struct SimulationResult {
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    pub fee_stroops: u64,
    pub chain: String,
    pub estimated_confirmation_seconds: u32,
}

#[derive(serde::Serialize)]
pub struct HealthStatus {
    pub status: &'static str,
    pub chains: Vec<&'static str>,
    pub payments_in_store: usize,
    pub provider_ready: bool,
    pub durable_store_ready: bool,
}
