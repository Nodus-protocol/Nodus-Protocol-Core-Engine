use dashmap::DashMap;
use serde_json::Value;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(86_400);

struct Entry {
    body: Value,
    stored_at: Instant,
}

#[derive(Default)]
pub struct IdempotencyStore(DashMap<String, Entry>);

impl IdempotencyStore {
    pub fn new() -> Self {
        Self(DashMap::new())
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.get(key).and_then(|e| {
            if e.stored_at.elapsed() < TTL {
                Some(e.body.clone())
            } else {
                None
            }
        })
    }

    pub fn set(&self, key: String, body: Value) {
        self.0.insert(
            key,
            Entry {
                body,
                stored_at: Instant::now(),
            },
        );
    }

    pub fn evict_expired(&self) {
        self.0.retain(|_, v| v.stored_at.elapsed() < TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stores_and_retrieves() {
        let store = IdempotencyStore::new();
        let key = "test-key".to_string();
        let val = json!({"id": "abc"});
        store.set(key.clone(), val.clone());
        assert_eq!(store.get(&key).unwrap(), val);
    }

    #[test]
    fn returns_none_for_missing_key() {
        let store = IdempotencyStore::new();
        assert!(store.get("missing").is_none());
    }
}
