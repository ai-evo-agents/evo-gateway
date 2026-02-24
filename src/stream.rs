use crate::error::GatewayError;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use reqwest::RequestBuilder;
use serde_json::Value;

/// Check if a request body has `"stream": true`.
pub fn is_streaming(body: &Value) -> bool {
    body.get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Proxy a streaming response from upstream, piping bytes directly without buffering.
///
/// On upstream error (non-2xx), falls back to buffered text for proper error reporting.
/// Forwards key upstream headers: content-type, rate-limit headers, x-request-id.
pub async fn proxy_streaming(req: RequestBuilder) -> Result<Response, GatewayError> {
    let response = req.send().await.map_err(GatewayError::from)?;
    let status = response.status();

    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(GatewayError::UpstreamError(format!(
            "upstream returned {status}: {text}"
        )));
    }

    // Build response headers
    let mut headers = HeaderMap::new();

    if let Some(ct) = response.headers().get("content-type") {
        headers.insert("content-type", ct.clone());
    }
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert("connection", HeaderValue::from_static("keep-alive"));

    // Forward rate-limit and request-id headers from upstream
    for name in [
        "x-request-id",
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
    ] {
        if let Some(val) = response.headers().get(name)
            && let Ok(header_name) = name.parse::<axum::http::HeaderName>()
        {
            headers.insert(header_name, val.clone());
        }
    }

    // Pipe upstream byte stream into axum response body without buffering
    let body = Body::from_stream(response.bytes_stream());

    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    *resp.headers_mut() = headers;

    Ok(resp)
}
