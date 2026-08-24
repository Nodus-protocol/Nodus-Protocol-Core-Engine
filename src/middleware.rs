use axum::{extract::Request, middleware::Next, response::Response};
use tracing::Instrument;
use uuid::Uuid;

#[derive(Clone)]
pub struct RequestId(#[allow(dead_code)] pub String);

pub async fn inject_request_id(mut req: Request, next: Next) -> Response {
    let endpoint = crate::observability::endpoint(req.uri().path());
    let id = req
        .headers()
        .get("x-correlation-id")
        .or_else(|| req.headers().get("x-request-id"))
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    req.extensions_mut().insert(RequestId(id.clone()));

    let started = crate::observability::request_started();
    let span = tracing::info_span!("http.request", correlation_id = %id, endpoint = %endpoint);
    let mut resp =
        crate::observability::correlated(id.clone(), next.run(req).instrument(span)).await;
    crate::observability::request_finished(started, !resp.status().is_success(), endpoint);
    if let Some(val) = crate::observability::header(&id) {
        resp.headers_mut().insert("x-correlation-id", val.clone());
        resp.headers_mut().insert("x-request-id", val);
    }
    resp
}
