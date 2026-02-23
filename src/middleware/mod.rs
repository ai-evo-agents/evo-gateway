use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use tracing::{info, warn};
use uuid::Uuid;

/// Injects X-Request-ID header and logs request/response timing.
pub async fn request_logging(mut req: Request, next: Next) -> Response {
    let request_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();

    req.headers_mut()
        .insert("x-request-id", HeaderValue::from_str(&request_id).unwrap());

    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %uri.path(),
    );

    let start = std::time::Instant::now();

    let response = {
        let _enter = span.enter();
        next.run(req).await
    };

    let duration = start.elapsed();
    let status = response.status().as_u16();

    if status >= 400 {
        warn!(
            request_id = %request_id,
            method = %method,
            path = %uri.path(),
            status = status,
            duration_ms = duration.as_millis(),
            "request completed with error"
        );
    } else {
        info!(
            request_id = %request_id,
            method = %method,
            path = %uri.path(),
            status = status,
            duration_ms = duration.as_millis(),
            "request completed"
        );
    }

    response
}
