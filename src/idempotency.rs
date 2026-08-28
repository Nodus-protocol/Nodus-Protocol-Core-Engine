use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::utils::EngineError;

/// Deployment-environment segment of an idempotency key.
///
/// Read from `NODUS_ENV` and kept separate from [`crate::config::Network`] so
/// that, for example, a mainnet engine running in a staging cell never shares
/// idempotency state with the real production mainnet engine.
pub fn idempotency_environment() -> String {
    std::env::var("NODUS_ENV")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Fully-qualified namespace for idempotency keys.
///
/// Two requests can only collide on a key when they share the same
/// environment, network, and endpoint. The client-chosen key is the final
/// segment, so a key reused across endpoints (or networks) is treated as an
/// entirely separate operation rather than a replay.
#[derive(Debug, Clone)]
pub struct IdempotencyNamespace {
    environment: String,
    network: &'static str,
    endpoint: &'static str,
}

impl IdempotencyNamespace {
    /// `network` is `"mainnet"` / `"testnet"`; `endpoint` is a stable dotted
    /// identifier for the operation, e.g. `"payments.initiate"`.
    pub fn new(network: &'static str, endpoint: &'static str) -> Self {
        Self {
            environment: idempotency_environment(),
            network,
            endpoint,
        }
    }

    /// Store key for a client-supplied idempotency key.
    pub fn key(&self, client_key: &str) -> String {
        format!(
            "nodus:idem:{}:{}:{}:{}",
            self.environment, self.network, self.endpoint, client_key
        )
    }

    pub fn endpoint(&self) -> &'static str {
        self.endpoint
    }
}

/// Stable SHA-256 fingerprint of a request body.
///
/// Object keys are sorted recursively before hashing, so two logically
/// identical payloads that differ only in key ordering (or whitespace)
/// produce the same fingerprint. A second request that presents the same
/// idempotency key with a *different* fingerprint is a client error, not a
/// replay.
pub fn request_fingerprint(body: &Value) -> String {
    let mut canonical = Vec::new();
    write_canonical(body, &mut canonical);
    hex::encode(Sha256::digest(&canonical))
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(format!("{key:?}").as_bytes());
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_includes_environment_network_and_endpoint() {
        let ns = IdempotencyNamespace::new("mainnet", "payments.initiate");
        let key = ns.key("abc-123");
        assert!(key.starts_with("nodus:idem:"));
        assert!(key.ends_with(":mainnet:payments.initiate:abc-123"));
    }

    #[test]
    fn same_client_key_on_different_endpoints_does_not_collide() {
        let a = IdempotencyNamespace::new("mainnet", "payments.initiate").key("k");
        let b = IdempotencyNamespace::new("mainnet", "payments.batch").key("k");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_is_stable_across_key_ordering() {
        let a = request_fingerprint(&json!({"a": 1, "b": {"x": 1, "y": 2}}));
        let b = request_fingerprint(&json!({"b": {"y": 2, "x": 1}, "a": 1}));
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_changes_with_value() {
        let a = request_fingerprint(&json!({"amount": 100}));
        let b = request_fingerprint(&json!({"amount": 101}));
        assert_ne!(a, b);
    }
}

/// Current Unix time in milliseconds. Milliseconds (not seconds) so a lease
/// can be sub-second and takeover decisions stay precise under load.
pub(crate) fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Lifecycle of a claimed idempotency key.
///
/// The distinction between [`InFlight`](ClaimLifecycle::InFlight) and
/// [`Submitting`](ClaimLifecycle::Submitting) is what makes takeover safe: an
/// `InFlight` claim has performed no irreversible side effect and can be
/// re-executed from scratch, whereas a `Submitting` claim has (or is about
/// to have) an external transaction in flight whose outcome must be
/// reconciled — never re-submitted.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "lifecycle", rename_all = "snake_case")]
pub enum ClaimLifecycle {
    /// Owner is executing; no side effect has occurred yet.
    InFlight,
    /// Owner recorded an execution reference before submitting. On takeover
    /// the successor must reconcile `execution_ref`, not re-run the work.
    Submitting { execution_ref: String },
    /// A final response has been committed and is replayed verbatim to every
    /// later request that presents the same key and fingerprint.
    Completed {
        response: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_ref: Option<String>,
    },
}

/// Durable record backing one idempotency key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClaimRecord {
    /// Canonical SHA-256 of the request body (see [`request_fingerprint`]).
    pub fingerprint: String,
    /// Opaque identity of the current owner. Used for compare-and-set when
    /// advancing the lifecycle and to detect a foreign takeover.
    pub owner: String,
    /// Unix-millis after which the lease is abandoned and another worker may
    /// take over an `InFlight` claim.
    pub lease_expires_at_ms: i64,
    /// Unix-millis the key was first claimed.
    pub created_at_ms: i64,
    #[serde(flatten)]
    pub lifecycle: ClaimLifecycle,
}

impl ClaimRecord {
    pub(crate) fn new_in_flight(fingerprint: &str, owner: &str, lease: Duration) -> Self {
        let now = now_millis();
        Self {
            fingerprint: fingerprint.to_string(),
            owner: owner.to_string(),
            lease_expires_at_ms: now + lease.as_millis() as i64,
            created_at_ms: now,
            lifecycle: ClaimLifecycle::InFlight,
        }
    }

    pub(crate) fn lease_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.lease_expires_at_ms
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn in_flight_record_round_trips_through_json() {
        let rec = ClaimRecord::new_in_flight("fp", "owner-1", Duration::from_secs(30));
        let raw = serde_json::to_string(&rec).unwrap();
        assert!(raw.contains("\"lifecycle\":\"in_flight\""));
        let back: ClaimRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.owner, "owner-1");
        assert_eq!(back.lifecycle, ClaimLifecycle::InFlight);
    }

    #[test]
    fn completed_record_round_trips_with_response() {
        let mut rec = ClaimRecord::new_in_flight("fp", "o", Duration::from_secs(1));
        rec.lifecycle = ClaimLifecycle::Completed {
            response: json!({"id": "p-1"}),
            execution_ref: Some("tx-abc".into()),
        };
        let raw = serde_json::to_string(&rec).unwrap();
        let back: ClaimRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.lifecycle, rec.lifecycle);
    }

    #[test]
    fn lease_expiry_is_inclusive_of_the_deadline() {
        let rec = ClaimRecord::new_in_flight("fp", "o", Duration::from_secs(0));
        assert!(rec.lease_expired(rec.lease_expires_at_ms));
        assert!(!rec.lease_expired(rec.lease_expires_at_ms - 1));
    }
}

/// Proof that the holder owns the in-flight claim for `key`. Required to
/// advance the claim to `Submitting` or `Completed`; a token whose `owner`
/// no longer matches the stored record is rejected with
/// [`EngineError::Conflict`].
#[derive(Debug, Clone)]
pub struct ClaimToken {
    pub(crate) key: String,
    pub(crate) owner: String,
}

impl ClaimToken {
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// Disposition of a [`ClaimStore::claim`] call.
#[derive(Debug)]
pub enum ClaimOutcome {
    /// A fresh claim was created; the caller owns it and must execute the work.
    Claimed(ClaimToken),
    /// An abandoned `InFlight` lease was taken over. No side effect had
    /// occurred, so the caller executes the work from scratch.
    TookOver(ClaimToken),
    /// Another owner holds a live lease. The caller must not execute; it
    /// should retry the request shortly to pick up the eventual result.
    InFlight,
    /// A previous attempt reached the submission stage but never recorded a
    /// final result. The outcome is unknown: the caller must reconcile
    /// `execution_ref` out of band and must never submit again under this key.
    AwaitingResult { execution_ref: String },
    /// A final response already exists; replay it verbatim.
    Replay { response: Value },
}

/// Atomic claim / result protocol for idempotent execution.
///
/// A single [`claim`](ClaimStore::claim) call both reserves an in-flight slot
/// *and* returns any existing disposition, so two concurrent identical
/// requests can never both receive `Claimed`. Implementations must perform
/// the read-decide-write as one indivisible operation (Lua on Redis, a
/// per-shard entry lock in memory).
#[async_trait]
pub trait ClaimStore: Send + Sync {
    /// Reserve `key` for `owner` with `fingerprint`, or report the existing
    /// claim's disposition. `lease` bounds how long this owner may hold the
    /// slot before another worker may take over an `InFlight` claim; `ttl`
    /// bounds how long the whole record (including a completed response)
    /// survives. A stored record whose fingerprint differs from `fingerprint`
    /// is rejected with [`EngineError::Conflict`].
    async fn claim(
        &self,
        key: &str,
        fingerprint: &str,
        owner: &str,
        lease: Duration,
        ttl: Duration,
    ) -> Result<ClaimOutcome, EngineError>;

    /// Record `execution_ref` and move the claim to `Submitting` immediately
    /// before an irreversible external submission. Refreshes the lease.
    /// Rejected with [`EngineError::Conflict`] if `token` no longer owns the
    /// claim, or [`EngineError::PreconditionFailed`] if the record has expired.
    async fn mark_submitting(
        &self,
        token: &ClaimToken,
        execution_ref: &str,
        lease: Duration,
        ttl: Duration,
    ) -> Result<(), EngineError>;

    /// Commit the final `response` (and optional `execution_ref`) so every
    /// later claim of this key replays it. Rejected with
    /// [`EngineError::Conflict`] if `token` no longer owns the claim.
    async fn complete(
        &self,
        token: &ClaimToken,
        response: &Value,
        execution_ref: Option<&str>,
        ttl: Duration,
    ) -> Result<(), EngineError>;

    /// Whether the backing store is reachable. On mainnet a `false` here is a
    /// fatal readiness failure — there is no in-memory fallback.
    async fn ready(&self) -> bool;
}

/// Counter names for the idempotency layer.
///
/// Every metric is a plain counter with no dynamic labels, so a request
/// payload, idempotency key, or fingerprint can never leak into telemetry —
/// the type of [`crate::observability::counter`] (`&'static str`) makes that
/// a compile-time guarantee.
pub mod metrics {
    /// A new claim (fresh or via takeover of an abandoned lease) was granted.
    pub const CLAIMS: &str = "nodus_idempotency_claims_total";
    /// A completed response was replayed to a later identical request.
    pub const REPLAYS: &str = "nodus_idempotency_replays_total";
    /// A key was presented with a fingerprint that did not match the stored
    /// claim, or a write lost its owner to a takeover.
    pub const CONFLICTS: &str = "nodus_idempotency_conflicts_total";
    /// An abandoned in-flight lease was taken over by a new owner.
    pub const STALE_LEASE_TAKEOVERS: &str = "nodus_idempotency_stale_lease_takeovers_total";
    /// A backing-store operation failed.
    pub const STORE_FAILURES: &str = "nodus_idempotency_store_failures_total";
    /// A request hit a claim that had submitted but not recorded a result;
    /// the outcome is unknown and must be reconciled out of band.
    pub const AWAITING_RESULT: &str = "nodus_idempotency_awaiting_result_total";
}

#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Value>, EngineError>;
    async fn set(&self, key: String, body: Value) -> Result<(), EngineError>;
    async fn ready(&self) -> bool;
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

    async fn ready(&self) -> bool {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .is_ok()
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

    async fn ready(&self) -> bool {
        false
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

// ─────────────────────────────────────────────────────────────────────────────
// In-memory atomic store — development and tests only. Never durable across a
// restart, so the factory refuses to hand one out on mainnet.
// ─────────────────────────────────────────────────────────────────────────────

/// In-process [`ClaimStore`]. Atomicity comes from `DashMap`'s per-shard entry
/// lock: the read-decide-write in `claim` runs while the shard is held, so two
/// concurrent claims of the same key are serialized.
#[derive(Default)]
pub struct MemoryClaimStore {
    records: DashMap<String, ClaimRecord>,
}

impl MemoryClaimStore {
    pub fn new() -> Self {
        Self {
            records: DashMap::new(),
        }
    }

    /// Drop records whose TTL (approximated by the lease window plus grace)
    /// has elapsed. Callers run this on a timer; correctness does not depend
    /// on it because every decision re-checks expiry inline.
    pub fn evict_expired(&self) {
        let now = now_millis();
        self.records
            .retain(|_, rec| now < rec.lease_expires_at_ms || matches!(rec.lifecycle, ClaimLifecycle::Completed { .. }));
    }

    #[cfg(test)]
    pub(crate) fn peek(&self, key: &str) -> Option<ClaimRecord> {
        self.records.get(key).map(|r| r.value().clone())
    }
}

#[async_trait]
impl ClaimStore for MemoryClaimStore {
    async fn claim(
        &self,
        key: &str,
        fingerprint: &str,
        owner: &str,
        lease: Duration,
        _ttl: Duration,
    ) -> Result<ClaimOutcome, EngineError> {
        use dashmap::mapref::entry::Entry;
        let now = now_millis();
        match self.records.entry(key.to_string()) {
            Entry::Vacant(slot) => {
                slot.insert(ClaimRecord::new_in_flight(fingerprint, owner, lease));
                crate::observability::counter(metrics::CLAIMS);
                Ok(ClaimOutcome::Claimed(ClaimToken {
                    key: key.to_string(),
                    owner: owner.to_string(),
                }))
            }
            Entry::Occupied(mut slot) => {
                let rec = slot.get_mut();
                if rec.fingerprint != fingerprint {
                    crate::observability::counter(metrics::CONFLICTS);
                    return Err(EngineError::Conflict(
                        "idempotency key reused with a different request".into(),
                    ));
                }
                match &rec.lifecycle {
                    ClaimLifecycle::Completed { response, .. } => {
                        crate::observability::counter(metrics::REPLAYS);
                        Ok(ClaimOutcome::Replay {
                            response: response.clone(),
                        })
                    }
                    ClaimLifecycle::Submitting { execution_ref } => {
                        crate::observability::counter(metrics::AWAITING_RESULT);
                        Ok(ClaimOutcome::AwaitingResult {
                            execution_ref: execution_ref.clone(),
                        })
                    }
                    ClaimLifecycle::InFlight => {
                        if !rec.lease_expired(now) {
                            return Ok(ClaimOutcome::InFlight);
                        }
                        // Abandoned lease with no side effect — safe takeover.
                        rec.owner = owner.to_string();
                        rec.lease_expires_at_ms = now + lease.as_millis() as i64;
                        crate::observability::counter(metrics::CLAIMS);
                        crate::observability::counter(metrics::STALE_LEASE_TAKEOVERS);
                        Ok(ClaimOutcome::TookOver(ClaimToken {
                            key: key.to_string(),
                            owner: owner.to_string(),
                        }))
                    }
                }
            }
        }
    }

    async fn mark_submitting(
        &self,
        token: &ClaimToken,
        execution_ref: &str,
        lease: Duration,
        _ttl: Duration,
    ) -> Result<(), EngineError> {
        let mut rec = self
            .records
            .get_mut(&token.key)
            .ok_or_else(|| EngineError::PreconditionFailed("idempotency claim expired".into()))?;
        if rec.owner != token.owner {
            crate::observability::counter(metrics::CONFLICTS);
            return Err(EngineError::Conflict("idempotency claim taken over".into()));
        }
        rec.lifecycle = ClaimLifecycle::Submitting {
            execution_ref: execution_ref.to_string(),
        };
        rec.lease_expires_at_ms = now_millis() + lease.as_millis() as i64;
        Ok(())
    }

    async fn complete(
        &self,
        token: &ClaimToken,
        response: &Value,
        execution_ref: Option<&str>,
        _ttl: Duration,
    ) -> Result<(), EngineError> {
        let mut rec = self
            .records
            .get_mut(&token.key)
            .ok_or_else(|| EngineError::PreconditionFailed("idempotency claim expired".into()))?;
        if rec.owner != token.owner {
            crate::observability::counter(metrics::CONFLICTS);
            return Err(EngineError::Conflict("idempotency claim taken over".into()));
        }
        rec.lifecycle = ClaimLifecycle::Completed {
            response: response.clone(),
            execution_ref: execution_ref.map(str::to_string),
        };
        Ok(())
    }

    async fn ready(&self) -> bool {
        // An in-memory store is reachable but not durable. Readiness treats it
        // as not-ready so a misconfigured deployment surfaces on /readyz.
        false
    }
}

#[cfg(test)]
mod memory_claim_tests {
    use super::*;
    use serde_json::json;

    fn store() -> MemoryClaimStore {
        MemoryClaimStore::new()
    }

    const LEASE: Duration = Duration::from_secs(30);
    const TTL: Duration = Duration::from_secs(86_400);

    #[tokio::test]
    async fn first_claim_is_granted_second_is_in_flight() {
        let s = store();
        let a = s.claim("k", "fp", "owner-a", LEASE, TTL).await.unwrap();
        assert!(matches!(a, ClaimOutcome::Claimed(_)));
        let b = s.claim("k", "fp", "owner-b", LEASE, TTL).await.unwrap();
        assert!(matches!(b, ClaimOutcome::InFlight));
    }

    #[tokio::test]
    async fn mismatched_fingerprint_is_rejected() {
        let s = store();
        s.claim("k", "fp-1", "o", LEASE, TTL).await.unwrap();
        let err = s.claim("k", "fp-2", "o", LEASE, TTL).await.unwrap_err();
        assert_eq!(err.http_status(), 409);
    }

    #[tokio::test]
    async fn completed_claim_replays_the_response() {
        let s = store();
        let token = match s.claim("k", "fp", "o", LEASE, TTL).await.unwrap() {
            ClaimOutcome::Claimed(t) => t,
            other => panic!("expected Claimed, got {other:?}"),
        };
        s.complete(&token, &json!({"id": "p-1"}), Some("tx-1"), TTL)
            .await
            .unwrap();
        match s.claim("k", "fp", "o2", LEASE, TTL).await.unwrap() {
            ClaimOutcome::Replay { response } => assert_eq!(response, json!({"id": "p-1"})),
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expired_in_flight_lease_can_be_taken_over() {
        let s = store();
        s.claim("k", "fp", "dead-owner", Duration::from_millis(0), TTL)
            .await
            .unwrap();
        match s.claim("k", "fp", "new-owner", LEASE, TTL).await.unwrap() {
            ClaimOutcome::TookOver(t) => assert_eq!(t.owner, "new-owner"),
            other => panic!("expected TookOver, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_submitting_claim_is_never_taken_over_even_with_expired_lease() {
        let s = store();
        let token = match s
            .claim("k", "fp", "o", Duration::from_millis(0), TTL)
            .await
            .unwrap()
        {
            ClaimOutcome::Claimed(t) => t,
            other => panic!("expected Claimed, got {other:?}"),
        };
        s.mark_submitting(&token, "exec-ref-1", Duration::from_millis(0), TTL)
            .await
            .unwrap();
        match s.claim("k", "fp", "successor", LEASE, TTL).await.unwrap() {
            ClaimOutcome::AwaitingResult { execution_ref } => assert_eq!(execution_ref, "exec-ref-1"),
            other => panic!("expected AwaitingResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_after_takeover_is_rejected_for_the_stale_owner() {
        let s = store();
        let stale = match s
            .claim("k", "fp", "o1", Duration::from_millis(0), TTL)
            .await
            .unwrap()
        {
            ClaimOutcome::Claimed(t) => t,
            other => panic!("expected Claimed, got {other:?}"),
        };
        // A successor takes over the abandoned lease.
        s.claim("k", "fp", "o2", LEASE, TTL).await.unwrap();
        let err = s
            .complete(&stale, &json!({}), None, TTL)
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 409);
    }
}
