use axum::{extract::Request, middleware::Next, response::Response};
use uuid::Uuid;

#[derive(Clone)]
pub struct RequestId(pub String);

pub async fn inject_request_id(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    req.extensions_mut().insert(RequestId(id.clone()));

    let mut resp = next.run(req).await;
    if let Ok(val) = id.parse() {
        resp.headers_mut().insert("x-request-id", val);
    }
    resp
}
