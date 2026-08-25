//! Per-workload, per-scope rate limiting.
//!
//! Applied *after* `auth::require_scope`, so limits are keyed by the
//! authenticated workload's key id rather than by IP (which is meaningless
//! behind a shared backend gateway). Each scope tier gets its own budget —
//! high-risk endpoints (submission, administration) are throttled harder
//! than reads.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use dashmap::DashMap;
use serde::Serialize;

use crate::auth::{Scope, ServiceIdentity};

#[derive(Clone, Copy)]
pub struct RateLimit {
    /// Requests allowed per `window`.
    pub limit: u32,
    pub window: Duration,
}

impl RateLimit {
    pub fn per_minute(limit: u32) -> Self {
        Self {
            limit,
            window: Duration::from_secs(60),
        }
    }
}

/// Default per-scope budgets. Overridable via env if a deployment needs
/// different headroom (see `Config::from_env`).
pub fn default_limit_for(scope: Scope) -> RateLimit {
    match scope {
        Scope::Read => RateLimit::per_minute(600),
        Scope::TxConstruct => RateLimit::per_minute(180),
        Scope::TxSubmit => RateLimit::per_minute(60),
        Scope::Admin => RateLimit::per_minute(30),
        Scope::Diagnostics => RateLimit::per_minute(30),
    }
}

struct Bucket {
    window_start: Instant,
    count: u32,
}

#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    fn allow(&self, key: &str, limit: &RateLimit) -> bool {
        let mut entry = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| Bucket {
                window_start: Instant::now(),
                count: 0,
            });

        if entry.window_start.elapsed() >= limit.window {
            entry.window_start = Instant::now();
            entry.count = 0;
        }

        if entry.count >= limit.limit {
            false
        } else {
            entry.count += 1;
            true
        }
    }
}

pub type RateLimiterState = Arc<RateLimiter>;

#[derive(Serialize)]
struct RateLimitError {
    code: &'static str,
    message: &'static str,
}

pub async fn enforce(
    State((limiter, scope)): State<(RateLimiterState, Scope)>,
    req: Request,
    next: Next,
) -> Response {
    let limit = default_limit_for(scope);

    // Keyed by authenticated workload when available (auth runs first in
    // the layer stack); falls back to the scope alone so unauthenticated
    // paths (there should be none reaching this layer) still get bounded.
    let key = match req.extensions().get::<ServiceIdentity>() {
        Some(identity) => format!("{}:{}", identity.key_id, scope),
        None => format!("anonymous:{scope}"),
    };

    if !limiter.allow(&key, &limit) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(RateLimitError {
                code: "RATE_LIMITED",
                message: "too many requests for this scope; slow down",
            }),
        )
            .into_response();
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            limit: 3,
            window: Duration::from_secs(60),
        };
        assert!(limiter.allow("k", &limit));
        assert!(limiter.allow("k", &limit));
        assert!(limiter.allow("k", &limit));
        assert!(!limiter.allow("k", &limit));
    }

    #[test]
    fn separate_keys_have_separate_budgets() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            limit: 1,
            window: Duration::from_secs(60),
        };
        assert!(limiter.allow("a", &limit));
        assert!(limiter.allow("b", &limit));
        assert!(!limiter.allow("a", &limit));
    }
}
