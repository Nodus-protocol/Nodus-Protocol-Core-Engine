use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("payment not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("chain adapter error: {0}")]
    AdapterError(String),
    #[error("network error: {0}")]
    NetworkError(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl EngineError {
    pub fn http_status(&self) -> u16 {
        match self {
            EngineError::NotFound(_) => 404,
            EngineError::InvalidRequest(_) => 400,
            _ => 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Pending,
    Processing,
    Confirmed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    #[default]
    Standard,
    Fast,
    Urgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub amount: u64,
    pub token: String,
    pub status: PaymentStatus,
    pub tx_hash: Option<String>,
    pub fee_stroops: u64,
    pub urgency: Urgency,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeEstimate {
    pub standard_stroops: u64,
    pub fast_stroops: u64,
    pub urgent_stroops: u64,
    pub standard_seconds: u32,
    pub fast_seconds: u32,
    pub urgent_seconds: u32,
}

impl Default for FeeEstimate {
    fn default() -> Self {
        Self {
            standard_stroops: 100,
            fast_stroops: 250,
            urgent_stroops: 500,
            standard_seconds: 5,
            fast_seconds: 3,
            urgent_seconds: 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

pub fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}
