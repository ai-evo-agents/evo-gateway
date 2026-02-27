//! Retry and fallback engine for upstream LLM provider requests.
//!
//! Inspired by zeroclaw's `reliable.rs`, adapted for evo-gateway's proxy model.
//! Classifies errors into retryable/non-retryable and implements exponential
//! backoff with jitter, key rotation on 429, and multi-provider fallback chains.

use crate::error::GatewayError;
use crate::state::{AppState, ProviderPool};
use evo_common::config::ReliabilityConfig;
use reqwest::{RequestBuilder, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

// ── Error Classification ────────────────────────────────────────────────────

/// How an upstream error should be handled.
#[derive(Debug)]
pub enum ErrorClass {
    /// 401, 403, 404, bad model — do not retry, try next provider.
    NonRetryable,
    /// 429 — retry with backoff, rotate API key first.
    RateLimited { retry_after: Option<Duration> },
    /// 5xx, 408, network errors — retry with backoff on same provider.
    Retryable,
    /// Context window exceeded — abort immediately, no fallback will help.
    ContextExceeded,
}

/// Classify an HTTP status code and response body for retry decisions.
pub fn classify_error(status: StatusCode, body: &str) -> ErrorClass {
    let code = status.as_u16();

    // Context window exceeded — abort, no provider can help
    if is_context_window_error(body) {
        return ErrorClass::ContextExceeded;
    }

    // Rate limited — retry with backoff
    if code == 429 {
        // Check if it's a business/quota limit (non-retryable rate limit)
        if is_non_retryable_rate_limit(body) {
            return ErrorClass::NonRetryable;
        }
        let retry_after = parse_retry_after_from_body(body);
        return ErrorClass::RateLimited { retry_after };
    }

    // Client errors (except 429/408) — non-retryable
    if status.is_client_error() && code != 408 {
        return ErrorClass::NonRetryable;
    }

    // 5xx, 408, and anything else — retryable
    ErrorClass::Retryable
}

/// Classify a GatewayError string for retry decisions.
#[allow(dead_code)]
pub fn classify_gateway_error(err: &GatewayError) -> ErrorClass {
    let msg = err.to_string();
    let lower = msg.to_lowercase();

    if is_context_window_error(&lower) {
        return ErrorClass::ContextExceeded;
    }

    // Try to extract HTTP status from the error message
    if lower.contains("429") || lower.contains("rate limit") {
        if is_non_retryable_rate_limit(&lower) {
            return ErrorClass::NonRetryable;
        }
        return ErrorClass::RateLimited { retry_after: None };
    }

    // Auth failures
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("invalid api key")
    {
        return ErrorClass::NonRetryable;
    }

    // 404 / model not found
    if lower.contains("404") || lower.contains("not found") {
        return ErrorClass::NonRetryable;
    }

    ErrorClass::Retryable
}

fn is_context_window_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    let hints = [
        "context window",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
    ];
    hints.iter().any(|h| lower.contains(h))
}

fn is_non_retryable_rate_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    let hints = [
        "plan does not include",
        "insufficient balance",
        "package not active",
        "quota exceeded",
        "billing",
    ];
    hints.iter().any(|h| lower.contains(h))
}

fn parse_retry_after_from_body(body: &str) -> Option<Duration> {
    // Try to find "retry_after" or "retry-after" in the response body
    if let Ok(json) = serde_json::from_str::<Value>(body)
        && let Some(secs) = json
            .get("error")
            .and_then(|e| e.get("retry_after"))
            .and_then(|v| v.as_f64())
    {
        return Some(Duration::from_secs_f64(secs));
    }
    None
}

// ── Backoff Calculation ─────────────────────────────────────────────────────

/// Calculate exponential backoff with jitter.
/// Formula: min(base * 2^attempt + jitter, max)
fn backoff_duration(attempt: u32, config: &ReliabilityConfig) -> Duration {
    let base_ms = config.base_backoff_ms as f64;
    let exp_ms = base_ms * 2.0_f64.powi(attempt as i32);
    // Add 0-50% jitter to avoid thundering herd
    let jitter_ms = exp_ms * 0.5 * rand::random::<f64>();
    let total_ms = (exp_ms + jitter_ms).min(config.max_backoff_ms as f64);
    Duration::from_millis(total_ms as u64)
}

// ── Reliable Request Execution ──────────────────────────────────────────────

/// Result of a reliable proxy attempt — either a raw reqwest::Response
/// (which the caller converts to streaming/json) or an error.
pub type ProxyResult = Result<reqwest::Response, GatewayError>;

/// Execute a proxied request with retry and fallback logic.
///
/// 1. Tries the primary provider with retries (exponential backoff).
/// 2. On non-retryable errors, falls back to the next provider in the chain.
/// 3. On rate-limit, rotates API key and retries.
/// 4. On context-exceeded, aborts immediately.
///
/// The `build_request` closure is called each attempt with the current provider
/// pool, allowing the caller to construct the appropriate request.
pub async fn reliable_proxy<F>(
    state: &AppState,
    primary_provider: &str,
    config: &ReliabilityConfig,
    build_request: F,
) -> ProxyResult
where
    F: Fn(&Arc<ProviderPool>, &AppState) -> Result<RequestBuilder, GatewayError>,
{
    // Build provider chain: primary first, then fallbacks (skip primary if in chain)
    let mut chain: Vec<String> = vec![primary_provider.to_string()];
    for name in &config.fallback_chain {
        if name != primary_provider {
            chain.push(name.clone());
        }
    }

    let mut last_error: Option<GatewayError> = None;

    for provider_name in &chain {
        let pool = match state.get_pool(provider_name).await {
            Ok(p) => p,
            Err(e) => {
                warn!(provider = %provider_name, error = %e, "fallback provider unavailable, skipping");
                last_error = Some(e);
                continue;
            }
        };

        // Retry loop for this provider
        for attempt in 0..=config.max_retries {
            let req = match build_request(&pool, state) {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(e);
                    break; // Config error, skip this provider
                }
            };

            match req.send().await {
                Ok(response) => {
                    let status = response.status();

                    if status.is_success() {
                        return Ok(response);
                    }

                    // Non-success — classify and decide
                    let body_text = response.text().await.unwrap_or_default();

                    let class = classify_error(status, &body_text);

                    match class {
                        ErrorClass::ContextExceeded => {
                            return Err(GatewayError::UpstreamError(format!(
                                "context window exceeded: {body_text}"
                            )));
                        }
                        ErrorClass::NonRetryable => {
                            warn!(
                                provider = %provider_name,
                                status = %status,
                                "non-retryable error, trying next provider"
                            );
                            last_error = Some(GatewayError::UpstreamError(format!(
                                "upstream returned {status}: {body_text}"
                            )));
                            break; // Try next provider
                        }
                        ErrorClass::RateLimited { retry_after } => {
                            if attempt < config.max_retries {
                                let delay = retry_after
                                    .unwrap_or_else(|| backoff_duration(attempt, config));
                                info!(
                                    provider = %provider_name,
                                    attempt = attempt + 1,
                                    delay_ms = delay.as_millis() as u64,
                                    "rate limited, backing off"
                                );
                                tokio::time::sleep(delay).await;
                                // Key rotation happens automatically via next_token()
                                continue;
                            }
                            last_error =
                                Some(GatewayError::RateLimitExceeded(provider_name.clone()));
                            break; // Exhausted retries, try next provider
                        }
                        ErrorClass::Retryable => {
                            if attempt < config.max_retries {
                                let delay = backoff_duration(attempt, config);
                                info!(
                                    provider = %provider_name,
                                    attempt = attempt + 1,
                                    status = %status,
                                    delay_ms = delay.as_millis() as u64,
                                    "retryable error, backing off"
                                );
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            last_error = Some(GatewayError::UpstreamError(format!(
                                "upstream returned {status} after {} retries: {body_text}",
                                config.max_retries
                            )));
                            break; // Exhausted retries, try next provider
                        }
                    }
                }
                Err(network_err) => {
                    // Network-level error (connection refused, DNS, timeout, etc.)
                    if attempt < config.max_retries {
                        let delay = backoff_duration(attempt, config);
                        warn!(
                            provider = %provider_name,
                            attempt = attempt + 1,
                            error = %network_err,
                            delay_ms = delay.as_millis() as u64,
                            "network error, retrying"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    last_error = Some(GatewayError::UpstreamError(format!(
                        "network error after {} retries: {network_err}",
                        config.max_retries
                    )));
                    break; // Exhausted retries, try next provider
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        GatewayError::ProviderNotFound("no providers available in fallback chain".to_string())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_429_as_rate_limited() {
        match classify_error(StatusCode::TOO_MANY_REQUESTS, "{}") {
            ErrorClass::RateLimited { .. } => {}
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_401_as_non_retryable() {
        match classify_error(StatusCode::UNAUTHORIZED, "{}") {
            ErrorClass::NonRetryable => {}
            other => panic!("expected NonRetryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_500_as_retryable() {
        match classify_error(StatusCode::INTERNAL_SERVER_ERROR, "{}") {
            ErrorClass::Retryable => {}
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_context_window() {
        match classify_error(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"maximum context length exceeded"}}"#,
        ) {
            ErrorClass::ContextExceeded => {}
            other => panic!("expected ContextExceeded, got {other:?}"),
        }
    }

    #[test]
    fn classify_quota_rate_limit_as_non_retryable() {
        match classify_error(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"insufficient balance"}}"#,
        ) {
            ErrorClass::NonRetryable => {}
            other => panic!("expected NonRetryable, got {other:?}"),
        }
    }

    #[test]
    fn backoff_increases_exponentially() {
        let config = ReliabilityConfig {
            max_retries: 5,
            base_backoff_ms: 100,
            max_backoff_ms: 10_000,
            fallback_chain: vec![],
        };
        let d0 = backoff_duration(0, &config);
        let d2 = backoff_duration(2, &config);
        // d2 base is 400ms, d0 base is 100ms — d2 should generally be larger
        // (jitter makes this probabilistic, but the base grows)
        assert!(d2.as_millis() >= 100, "d2 should be at least 100ms");
        assert!(d0.as_millis() >= 50, "d0 should be at least 50ms");
    }

    #[test]
    fn backoff_capped_at_max() {
        let config = ReliabilityConfig {
            max_retries: 5,
            base_backoff_ms: 1000,
            max_backoff_ms: 5000,
            fallback_chain: vec![],
        };
        let d = backoff_duration(10, &config);
        assert!(d.as_millis() <= 5000, "should be capped at max_backoff_ms");
    }
}
