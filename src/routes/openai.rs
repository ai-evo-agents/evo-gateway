//! Unified handler for all LLM completion requests.
//!
//! Request model field format:  `"provider:model"` or just `"model"`.
//!
//! Examples:
//!   "openai:gpt-4o"                           → OpenAI pool, model=gpt-4o
//!   "anthropic:claude-3-5-sonnet-20241022"     → Anthropic pool
//!   "openrouter:anthropic/claude-3.5-sonnet"  → OpenRouter pool (OR keeps full path)
//!   "ollama:llama3.2"                          → local Ollama
//!   "gpt-4o"                                   → auto-route (first enabled pool)

use crate::{
    error::GatewayError,
    state::AppState,
    stream::{is_streaming, proxy_streaming},
};
use axum::{Json, extract::State, response::IntoResponse, response::Response};
use evo_common::config::ProviderType;
use reqwest::RequestBuilder;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::instrument;

// ─── Public route handlers ────────────────────────────────────────────────────

/// POST /v1/chat/completions
/// Dispatches based on `provider:model` syntax in the `model` field.
/// Supports streaming (`"stream": true`) — returns SSE events without buffering.
#[instrument(skip(state, body), fields(model))]
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(mut body): Json<Value>,
) -> Result<Response, GatewayError> {
    let model_str = body["model"].as_str().unwrap_or("").to_string();

    let (provider_name, actual_model) = parse_provider_model(&model_str);
    tracing::Span::current().record("model", actual_model);

    let pool = match provider_name {
        Some(name) => state.get_pool(name).await?,
        None => {
            state
                .get_preferred_pool(&["openai", "openrouter", "anthropic", "ollama"])
                .await?
        }
    };

    // Rewrite model field — strip the `provider:` prefix before forwarding
    body["model"] = json!(actual_model);

    match pool.provider_type() {
        ProviderType::OpenAiCompatible => {
            let url = format!("{}/chat/completions", pool.config.base_url);
            let req = build_openai_request(&state, &pool, &url, &body)?;

            if is_streaming(&body) {
                proxy_streaming(req).await
            } else {
                proxy_json(req).await.map(IntoResponse::into_response)
            }
        }
        ProviderType::Anthropic => anthropic_chat(&state, &pool, body).await,
        ProviderType::Cursor => {
            if is_streaming(&body) {
                crate::cursor::cursor_chat_streaming(&body, actual_model).await
            } else {
                let json = crate::cursor::cursor_chat(&body, actual_model).await?;
                Ok(Json(json).into_response())
            }
        }
    }
}

/// POST /v1/embeddings
#[instrument(skip(state, body))]
pub async fn embeddings(
    State(state): State<Arc<AppState>>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let model_str = body["model"].as_str().unwrap_or("").to_string();
    let (provider_name, actual_model) = parse_provider_model(&model_str);

    let pool = match provider_name {
        Some(name) => state.get_pool(name).await?,
        None => {
            state
                .get_preferred_pool(&["openai", "openrouter", "ollama"])
                .await?
        }
    };

    body["model"] = json!(actual_model);
    let url = format!("{}/embeddings", pool.config.base_url);
    let req = build_openai_request(&state, &pool, &url, &body)?;
    proxy_json(req).await
}

/// GET /v1/models — list all enabled providers as model entries
#[instrument(skip(state))]
pub async fn list_models(State(state): State<Arc<AppState>>) -> Result<Json<Value>, GatewayError> {
    let pools = state.all_enabled_pools().await;

    let models: Vec<Value> = pools
        .iter()
        .map(|p| {
            json!({
                "id": p.name(),
                "object": "model",
                "owned_by": "evo-gateway",
                "tokens_available": p.token_pool.len(),
            })
        })
        .collect();

    Ok(Json(json!({
        "object": "list",
        "data": models,
    })))
}

// ─── Provider-specific proxy logic ───────────────────────────────────────────

/// Build an OpenAI-compatible request (Bearer auth + extra_headers).
fn build_openai_request(
    state: &AppState,
    pool: &crate::state::ProviderPool,
    url: &str,
    body: &Value,
) -> Result<RequestBuilder, GatewayError> {
    let token = pool.token_pool.next_token();
    let mut req = state.http_client.post(url).json(body);

    if let Some(t) = token {
        req = req.bearer_auth(t);
    }

    // Inject provider-specific extra headers (e.g. OpenRouter's HTTP-Referer)
    for (k, v) in &pool.config.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    Ok(req)
}

/// Anthropic uses `x-api-key` + `anthropic-version` and a slightly different schema.
/// Handles both streaming and non-streaming requests.
async fn anthropic_chat(
    state: &AppState,
    pool: &crate::state::ProviderPool,
    body: Value,
) -> Result<Response, GatewayError> {
    let token = pool.token_pool.next_token().ok_or_else(|| {
        GatewayError::ConfigError(format!(
            "no API token configured for provider '{}'",
            pool.name()
        ))
    })?;

    let url = format!("{}/messages", pool.config.base_url);

    let mut req = state
        .http_client
        .post(&url)
        .header("x-api-key", token)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");

    for (k, v) in &pool.config.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    if is_streaming(&body) {
        proxy_streaming(req.json(&body)).await
    } else {
        proxy_json(req.json(&body))
            .await
            .map(IntoResponse::into_response)
    }
}

/// Execute a prepared request and decode the JSON response.
async fn proxy_json(req: RequestBuilder) -> Result<Json<Value>, GatewayError> {
    let response = req.send().await.map_err(GatewayError::from)?;
    let status = response.status();
    let json: Value = response
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    if !status.is_success() {
        return Err(GatewayError::UpstreamError(format!(
            "upstream returned {status}: {json}"
        )));
    }
    Ok(Json(json))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Parse `"provider:model"` → `(Some("provider"), "model")`.
/// Bare `"model"` → `(None, "model")`.
fn parse_provider_model(s: &str) -> (Option<&str>, &str) {
    match s.split_once(':') {
        Some((provider, model)) => (Some(provider), model),
        None => (None, s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_provider() {
        let (p, m) = parse_provider_model("openai:gpt-4o");
        assert_eq!(p, Some("openai"));
        assert_eq!(m, "gpt-4o");
    }

    #[test]
    fn parse_openrouter_path() {
        let (p, m) = parse_provider_model("openrouter:anthropic/claude-3.5-sonnet");
        assert_eq!(p, Some("openrouter"));
        assert_eq!(m, "anthropic/claude-3.5-sonnet");
    }

    #[test]
    fn parse_bare_model() {
        let (p, m) = parse_provider_model("gpt-4o-mini");
        assert_eq!(p, None);
        assert_eq!(m, "gpt-4o-mini");
    }
}
