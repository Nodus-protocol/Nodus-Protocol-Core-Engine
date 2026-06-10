pub mod batch;
pub mod fees;
pub mod health;
pub mod payments;
pub mod rates;
pub mod webhooks;

use std::sync::Arc;
use crate::engine::Engine;
use crate::rates::RateService;
use crate::webhook::WebhookStore;

pub struct AppContext {
    pub engine: Arc<Engine>,
    pub rates: RateService,
    pub webhooks: Arc<WebhookStore>,
}

pub type AppState = Arc<AppContext>;
