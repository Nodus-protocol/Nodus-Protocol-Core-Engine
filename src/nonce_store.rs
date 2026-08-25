//! Durable replay-protection store for signed engine requests.
//!
//! Every authenticated request carries a caller-chosen nonce. `check_and_set`
//! atomically records "this nonce has been seen for this key" and reports
//! whether it was already present — a `true` result means the request is a
//! replay and must be rejected. Entries expire after the configured replay
//! window so the store does not grow without bound.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

use crate::utils::EngineError;

#[async_trait]
pub trait NonceStore: Send + Sync {
    /// Records `key` (already namespaced by caller + nonce) if absent.
    /// Returns `true` if the key was already present (i.e. a replay).
    async fn check_and_set(&self, key: &str, ttl: Duration) -> Result<bool, EngineError>;
}

pub struct RedisNonceStore {
    conn: ConnectionManager,
}

impl RedisNonceStore {
    pub async fn new(redis_url: &str) -> Result<Self, EngineError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| EngineError::Internal(format!("redis client error: {e}")))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| EngineError::Internal(format!("redis connect error: {e}")))?;
        Ok(Self { conn })
    }
}

#[async_trait]
impl NonceStore for RedisNonceStore {
    async fn check_and_set(&self, key: &str, ttl: Duration) -> Result<bool, EngineError> {
        let mut conn = self.conn.clone();
        let redis_key = format!("nonce:{key}");
        // SET key val NX EX ttl — succeeds only if the key did not already exist.
        let opts = redis::SetOptions::default()
            .conditional_set(redis::ExistenceCheck::NX)
            .with_expiration(redis::SetExpiry::EX(ttl.as_secs().max(1)));
        let set: Option<String> = conn
            .set_options(&redis_key, "1", opts)
            .await
            .map_err(|e| EngineError::Internal(format!("redis nonce set error: {e}")))?;
        // `set` is `None` when NX prevented the write — i.e. the nonce was already used.
        Ok(set.is_none())
    }
}

struct Entry {
    stored_at: std::time::Instant,
}

pub struct MemoryNonceStore {
    entries: DashMap<String, Entry>,
}

impl Default for MemoryNonceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryNonceStore {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    pub fn evict_expired(&self, ttl: Duration) {
        self.entries.retain(|_, v| v.stored_at.elapsed() < ttl);
    }
}

#[async_trait]
impl NonceStore for MemoryNonceStore {
    async fn check_and_set(&self, key: &str, ttl: Duration) -> Result<bool, EngineError> {
        // dashmap's entry API makes the check-then-insert atomic per shard,
        // which is what gives us replay safety under concurrent requests.
        use dashmap::mapref::entry::Entry as DashEntry;
        match self.entries.entry(key.to_string()) {
            DashEntry::Occupied(mut o) => {
                if o.get().stored_at.elapsed() >= ttl {
                    // Expired — treat as fresh and reset it.
                    o.insert(Entry {
                        stored_at: std::time::Instant::now(),
                    });
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            DashEntry::Vacant(v) => {
                v.insert(Entry {
                    stored_at: std::time::Instant::now(),
                });
                Ok(false)
            }
        }
    }
}

pub async fn create_nonce_store(
    redis_url: Option<&str>,
    replay_window: Duration,
) -> (Arc<dyn NonceStore>, Option<tokio::task::JoinHandle<()>>) {
    match redis_url {
        Some(url) => match RedisNonceStore::new(url).await {
            Ok(store) => {
                tracing::info!("nonce store: redis");
                (Arc::new(store), None)
            }
            Err(e) => {
                tracing::warn!(error = %e, "redis unavailable, falling back to in-memory nonce store");
                let store = Arc::new(MemoryNonceStore::new());
                let handle = spawn_memory_eviction(store.clone(), replay_window);
                (store, Some(handle))
            }
        },
        None => {
            tracing::info!("nonce store: in-memory (replay protection reset on restart)");
            let store = Arc::new(MemoryNonceStore::new());
            let handle = spawn_memory_eviction(store.clone(), replay_window);
            (store, Some(handle))
        }
    }
}

fn spawn_memory_eviction(
    store: Arc<MemoryNonceStore>,
    ttl: Duration,
) -> tokio::task::JoinHandle<()> {
    let interval = (ttl / 4).max(Duration::from_secs(1));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            store.evict_expired(ttl);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_use_is_accepted() {
        let store = MemoryNonceStore::new();
        let replay = store
            .check_and_set("k1:nonce-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!replay);
    }

    #[tokio::test]
    async fn reuse_is_flagged_as_replay() {
        let store = MemoryNonceStore::new();
        store
            .check_and_set("k1:nonce-1", Duration::from_secs(60))
            .await
            .unwrap();
        let replay = store
            .check_and_set("k1:nonce-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(replay);
    }

    #[tokio::test]
    async fn expired_entry_is_treated_as_fresh() {
        let store = MemoryNonceStore::new();
        store
            .check_and_set("k1:nonce-1", Duration::from_secs(0))
            .await
            .unwrap();
        // ttl of 0 means "already expired" on the very next check.
        let replay = store
            .check_and_set("k1:nonce-1", Duration::from_secs(0))
            .await
            .unwrap();
        assert!(!replay);
    }

    #[tokio::test]
    async fn distinct_keys_do_not_collide() {
        let store = MemoryNonceStore::new();
        store
            .check_and_set("k1:nonce-1", Duration::from_secs(60))
            .await
            .unwrap();
        let replay = store
            .check_and_set("k2:nonce-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!replay);
    }
}
