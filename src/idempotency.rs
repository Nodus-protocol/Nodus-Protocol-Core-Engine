use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value;

use crate::utils::EngineError;

#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Value>, EngineError>;
    async fn set(&self, key: String, body: Value) -> Result<(), EngineError>;
}

pub struct RedisIdempotencyStore {
    conn: ConnectionManager,
    ttl: Duration,
}

impl RedisIdempotencyStore {
    pub async fn new(redis_url: &str, ttl: Duration) -> Result<Self, EngineError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| EngineError::Internal(format!("redis client error: {e}")))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| EngineError::Internal(format!("redis connect error: {e}")))?;
        Ok(Self { conn, ttl })
    }
}

#[async_trait]
impl IdempotencyStore for RedisIdempotencyStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, EngineError> {
        let mut conn = self.conn.clone();
        let redis_key = format!("idem:{key}");
        let raw: Option<String> = conn
            .get(&redis_key)
            .await
            .map_err(|e| EngineError::Internal(format!("redis get error: {e}")))?;
        match raw {
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| EngineError::Internal(format!("redis deserialize error: {e}"))),
            None => Ok(None),
        }
    }

    async fn set(&self, key: String, body: Value) -> Result<(), EngineError> {
        let mut conn = self.conn.clone();
        let redis_key = format!("idem:{key}");
        let serialized = serde_json::to_string(&body)
            .map_err(|e| EngineError::Internal(format!("redis serialize error: {e}")))?;
        let _: () = conn
            .set_ex(&redis_key, serialized, self.ttl.as_secs())
            .await
            .map_err(|e| EngineError::Internal(format!("redis set error: {e}")))?;
        Ok(())
    }
}

struct Entry {
    body: Value,
    stored_at: std::time::Instant,
}

pub struct MemoryIdempotencyStore {
    entries: DashMap<String, Entry>,
    ttl: Duration,
}

impl MemoryIdempotencyStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
        }
    }

    pub fn evict_expired(&self) {
        self.entries.retain(|_, v| v.stored_at.elapsed() < self.ttl);
    }
}

#[async_trait]
impl IdempotencyStore for MemoryIdempotencyStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, EngineError> {
        Ok(self.entries.get(key).and_then(|e| {
            if e.stored_at.elapsed() < self.ttl {
                Some(e.body.clone())
            } else {
                None
            }
        }))
    }

    async fn set(&self, key: String, body: Value) -> Result<(), EngineError> {
        self.entries.insert(
            key,
            Entry {
                body,
                stored_at: std::time::Instant::now(),
            },
        );
        Ok(())
    }
}

pub async fn create_idempotency_store(
    redis_url: Option<&str>,
    ttl: Duration,
) -> (Arc<dyn IdempotencyStore>, tokio::task::JoinHandle<()>) {
    match redis_url {
        Some(url) => match RedisIdempotencyStore::new(url, ttl).await {
            Ok(store) => {
                tracing::info!("idempotency store: redis");
                let noop = tokio::spawn(async {});
                (Arc::new(store), noop)
            }
            Err(e) => {
                tracing::warn!(error = %e, "redis unavailable, falling back to in-memory idempotency store");
                let store = Arc::new(MemoryIdempotencyStore::new(ttl));
                let handle = spawn_memory_eviction(store.clone(), ttl);
                (store, handle)
            }
        },
        None => {
            tracing::info!("idempotency store: in-memory (keys lost on restart)");
            let store = Arc::new(MemoryIdempotencyStore::new(ttl));
            let handle = spawn_memory_eviction(store.clone(), ttl);
            (store, handle)
        }
    }
}

fn spawn_memory_eviction(
    store: Arc<MemoryIdempotencyStore>,
    ttl: Duration,
) -> tokio::task::JoinHandle<()> {
    let interval = ttl / 4;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            store.evict_expired();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn memory_stores_and_retrieves() {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(86_400));
        let val = json!({"id": "abc"});
        store
            .set("test-key".to_string(), val.clone())
            .await
            .unwrap();
        assert_eq!(store.get("test-key").await.unwrap().unwrap(), val);
    }

    #[tokio::test]
    async fn memory_returns_none_for_missing_key() {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(86_400));
        assert!(store.get("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_evicts_expired_entries() {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(0));
        store.set("key".to_string(), json!("val")).await.unwrap();
        store.evict_expired();
        assert!(store.get("key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_overwrites_existing_key() {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(86_400));
        store.set("k".to_string(), json!("v1")).await.unwrap();
        store.set("k".to_string(), json!("v2")).await.unwrap();
        assert_eq!(store.get("k").await.unwrap().unwrap(), json!("v2"));
    }
}
