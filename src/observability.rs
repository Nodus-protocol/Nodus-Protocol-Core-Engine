use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use axum::http::HeaderValue;

#[derive(Clone)]
pub struct Identity {
    pub network: String,
    pub provider: String,
    pub release: String,
    pub manifest: String,
    pub contract: String,
}

#[derive(Default)]
struct Values {
    counters: HashMap<&'static str, u64>,
    gauges: HashMap<&'static str, f64>,
    requests: HashMap<String, RequestMetrics>,
}

#[derive(Default)]
struct RequestMetrics {
    count: u64,
    errors: u64,
    latency_sum: f64,
    buckets: [u64; 6],
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();
static VALUES: LazyLock<Mutex<Values>> = LazyLock::new(|| Mutex::new(Values::default()));
static INFLIGHT: AtomicU64 = AtomicU64::new(0);

tokio::task_local! { static CORRELATION_ID: String; }

pub fn init(identity: Identity) {
    let _ = IDENTITY.set(identity);
}

pub async fn correlated<F: std::future::Future>(id: String, future: F) -> F::Output {
    CORRELATION_ID.scope(id, future).await
}

pub fn correlation_id() -> Option<String> {
    CORRELATION_ID.try_with(Clone::clone).ok()
}

pub fn propagate(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match correlation_id() {
        Some(id) => builder.header("x-correlation-id", id),
        None => builder,
    }
}

pub fn counter(name: &'static str) {
    *VALUES
        .lock()
        .expect("metrics lock")
        .counters
        .entry(name)
        .or_default() += 1;
}

pub fn gauge(name: &'static str, value: f64) {
    VALUES
        .lock()
        .expect("metrics lock")
        .gauges
        .insert(name, value);
}

pub fn request_started() -> Instant {
    gauge(
        "nodus_queue_depth",
        INFLIGHT.fetch_add(1, Ordering::Relaxed) as f64 + 1.0,
    );
    Instant::now()
}

pub fn request_finished(started: Instant, failed: bool, endpoint: String) {
    let mut values = VALUES.lock().expect("metrics lock");
    let request = values.requests.entry(endpoint).or_default();
    let latency = started.elapsed().as_secs_f64();
    request.count += 1;
    request.errors += u64::from(failed);
    request.latency_sum += latency;
    for (index, boundary) in [0.05, 0.1, 0.25, 0.5, 1.0, 5.0].iter().enumerate() {
        request.buckets[index] += u64::from(latency <= *boundary);
    }
    values.gauges.insert(
        "nodus_queue_depth",
        INFLIGHT.fetch_sub(1, Ordering::Relaxed).saturating_sub(1) as f64,
    );
}

pub fn encode(endpoint: &str) -> String {
    let identity = IDENTITY.get().cloned().unwrap_or(Identity {
        network: "unknown".into(),
        provider: "unknown".into(),
        release: "unknown".into(),
        manifest: "unknown".into(),
        contract: "unknown".into(),
    });
    let labels = |endpoint: &str| {
        format!(
        "network=\"{}\",provider=\"{}\",release=\"{}\",manifest=\"{}\",contract_version=\"{}\",endpoint=\"{}\"",
        safe(&identity.network), safe(&identity.provider), safe(&identity.release),
        safe(&identity.manifest), safe(&identity.contract), safe(endpoint),
    )
    };
    let values = VALUES.lock().expect("metrics lock");
    let mut output = String::new();
    for (name, value) in &values.counters {
        output.push_str(&format!("{name}{{{}}} {value}\n", labels(endpoint)));
    }
    for (name, value) in &values.gauges {
        output.push_str(&format!("{name}{{{}}} {value}\n", labels(endpoint)));
    }
    for (endpoint, request) in &values.requests {
        let labels = labels(endpoint);
        output.push_str(&format!(
            "nodus_http_requests_total{{{labels}}} {}\n",
            request.count
        ));
        output.push_str(&format!(
            "nodus_http_errors_total{{{labels}}} {}\n",
            request.errors
        ));
        output.push_str(&format!(
            "nodus_http_request_latency_seconds_sum{{{labels}}} {}\n",
            request.latency_sum
        ));
        output.push_str(&format!(
            "nodus_http_request_latency_seconds_count{{{labels}}} {}\n",
            request.count
        ));
        for (boundary, count) in ["0.05", "0.1", "0.25", "0.5", "1", "5"]
            .iter()
            .zip(request.buckets)
        {
            output.push_str(&format!(
                "nodus_http_request_latency_seconds_bucket{{{labels},le=\"{boundary}\"}} {count}\n"
            ));
        }
        output.push_str(&format!(
            "nodus_http_request_latency_seconds_bucket{{{labels},le=\"+Inf\"}} {}\n",
            request.count
        ));
    }
    output
}

fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect()
}

pub fn endpoint(path: &str) -> String {
    path.split('/')
        .map(|part| {
            if part.len() > 20 || part.parse::<u64>().is_ok() {
                ":id"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn header(id: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(id).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_schema_excludes_financial_and_identity_data() {
        let output = endpoint("/api/v1/payments/secret-account-value-that-is-private");
        for forbidden in [
            "token",
            "signature",
            "xdr",
            "amount",
            "balance",
            "secret-account",
        ] {
            assert!(!output.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[test]
    fn labels_are_bounded_and_sanitized() {
        assert_eq!(safe("mainnet\"}\nunsafe  value"), "mainnetunsafevalue");
    }

    #[tokio::test]
    async fn correlation_id_is_propagated_without_payload_data() {
        correlated("backend-123".into(), async {
            let request = propagate(reqwest::Client::new().get("http://localhost"))
                .build()
                .unwrap();
            assert_eq!(request.headers()["x-correlation-id"], "backend-123");
        })
        .await;
    }
}
