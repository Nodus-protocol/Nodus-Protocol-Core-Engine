pub mod batch;
pub mod fees;
pub mod health;
pub mod payments;
pub mod pool;
pub mod rates;
pub mod webhooks;

use std::sync::Arc;

use crate::engine::Engine;
use crate::pool::ContractClient;
use crate::rates::RateService;
use crate::webhook::WebhookStore;

pub struct AppContext {
    pub engine: Arc<Engine>,
    pub rates: RateService,
    pub webhooks: Arc<WebhookStore>,
    pub pool: Option<ContractClient>,
}

pub type AppState = Arc<AppContext>;
