use crate::utils::{now_utc, EngineError, Payment, PaymentStatus};
use dashmap::DashMap;

#[derive(Default)]
pub struct PaymentStore(DashMap<String, Payment>);

impl PaymentStore {
    pub fn new() -> Self {
        Self(DashMap::new())
    }

    pub fn insert(&self, payment: Payment) {
        self.0.insert(payment.id.clone(), payment);
    }

    pub fn get(&self, id: &str) -> Result<Payment, EngineError> {
        self.0
            .get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| EngineError::NotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<Payment> {
        self.0.iter().map(|r| r.value().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn set_status(&self, id: &str, status: PaymentStatus) -> Result<(), EngineError> {
        let mut payment = self.get(id)?;
        payment.status = status;
        payment.updated_at = now_utc();
        self.0.insert(id.to_string(), payment);
        Ok(())
    }

    pub fn set_confirmed(&self, id: &str, tx_hash: String) -> Result<(), EngineError> {
        let mut payment = self.get(id)?;
        payment.status = PaymentStatus::Confirmed;
        payment.tx_hash = Some(tx_hash);
        payment.updated_at = now_utc();
        self.0.insert(id.to_string(), payment);
        Ok(())
    }

    pub fn set_failed(&self, id: &str, reason: String) -> Result<(), EngineError> {
        let mut payment = self.get(id)?;
        payment.status = PaymentStatus::Failed;
        payment.error = Some(reason);
        payment.updated_at = now_utc();
        self.0.insert(id.to_string(), payment);
        Ok(())
    }
}
