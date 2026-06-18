use dashmap::DashMap;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::utils::{now_utc, EngineError, Payment};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEvent {
    PaymentConfirmed,
    PaymentFailed,
    PaymentPending,
}

impl WebhookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PaymentConfirmed => "payment.confirmed",
            Self::PaymentFailed => "payment.failed",
            Self::PaymentPending => "payment.pending",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    #[serde(skip_serializing)]
    pub secret: String,
    pub events: Vec<WebhookEvent>,
    pub active: bool,
    pub created_at: String,
}

#[derive(Serialize)]
struct Payload<'a> {
    event: &'static str,
    payment: &'a Payment,
    delivered_at: String,
}

pub struct WebhookStore {
    hooks: DashMap<String, Webhook>,
    client: reqwest::Client,
}

impl WebhookStore {
    pub fn new() -> Self {
        Self {
            hooks: DashMap::new(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("webhook HTTP client"),
        }
    }

    pub fn register(&self, url: String, secret: String, events: Vec<WebhookEvent>) -> Webhook {
        let hook = Webhook {
            id: Uuid::new_v4().to_string(),
            url,
            secret,
            events,
            active: true,
            created_at: now_utc(),
        };
        self.hooks.insert(hook.id.clone(), hook.clone());
        hook
    }

    pub fn list(&self) -> Vec<Webhook> {
        self.hooks.iter().map(|r| r.value().clone()).collect()
    }

    pub fn get(&self, id: &str) -> Result<Webhook, EngineError> {
        self.hooks
            .get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| EngineError::NotFound(id.to_string()))
    }

    pub fn delete(&self, id: &str) -> Result<(), EngineError> {
        self.hooks
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| EngineError::NotFound(id.to_string()))
    }

    pub fn set_active(&self, id: &str, active: bool) -> Result<(), EngineError> {
        let mut hook = self.get(id)?;
        hook.active = active;
        self.hooks.insert(id.to_string(), hook);
        Ok(())
    }

    pub async fn dispatch(&self, event: WebhookEvent, payment: &Payment) {
        let event_str = event.as_str();

        for entry in self.hooks.iter() {
            let hook = entry.value();

            if !hook.active || !hook.events.contains(&event) {
                continue;
            }

            let body = match serde_json::to_string(&Payload {
                event: event_str,
                payment,
                delivered_at: now_utc(),
            }) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(webhook_id = %hook.id, "serialization error: {e}");
                    continue;
                }
            };

            let sig = hmac_sign(&hook.secret, &body);

            match self
                .client
                .post(&hook.url)
                .header("content-type", "application/json")
                .header("x-nodus-signature", &sig)
                .body(body)
                .send()
                .await
            {
                Ok(resp) => tracing::info!(
                    webhook_id = %hook.id,
                    status = %resp.status(),
                    "webhook delivered"
                ),
                Err(e) => tracing::warn!(webhook_id = %hook.id, error = %e, "webhook failed"),
            }
        }
    }
}

fn hmac_sign(secret: &str, body: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
