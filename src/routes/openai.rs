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
    reliability,
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
/// Supports `hint:<name>` prefix for routing via the `routing` config.
/// Supports streaming (`"stream": true`) — returns SSE events without buffering.
/// When reliability config is set, uses retry/fallback for non-streaming API requests.
#[instrument(skip(state, body), fields(model))]
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(mut body): Json<Value>,
) -> Result<Response, GatewayError> {
    let model_str = body["model"].as_str().unwrap_or("").to_string();

    let (provider_name, actual_model) = resolve_model(&model_str, &state);
    tracing::Span::current().record("model", actual_model.as_str());

    let pool = match provider_name.as_deref() {
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
            // For non-streaming + reliability config, use the retry/fallback engine
            if !is_streaming(&body)
                && let Some(ref reliability_config) = state.reliability
            {
                let body_clone = body.clone();
                let response =
                    reliability::reliable_proxy(&state, pool.name(), reliability_config, |p, s| {
                        let url = format!("{}/chat/completions", p.config.base_url);
                        build_openai_request(s, p, &url, &body_clone)
                    })
                    .await?;

                let json: Value = response
                    .json()
                    .await
                    .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;
                return Ok(Json(json).into_response());
            }

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
            let tmux = state.tmux_manager.as_deref();
            if is_streaming(&body) {
                crate::cursor::cursor_chat_streaming(&body, &actual_model, tmux).await
            } else {
                let json = crate::cursor::cursor_chat(&body, &actual_model, tmux).await?;
                Ok(Json(json).into_response())
            }
        }
        ProviderType::ClaudeCode => {
            let tmux = state.tmux_manager.as_deref();
            if is_streaming(&body) {
                crate::claude_code::claude_code_chat_streaming(&body, &actual_model, tmux).await
            } else {
                let json = crate::claude_code::claude_code_chat(&body, &actual_model, tmux).await?;
                Ok(Json(json).into_response())
            }
        }
        ProviderType::CodexCli => {
            let tmux = state.tmux_manager.as_deref();
            if is_streaming(&body) {
                crate::codex_cli::codex_cli_chat_streaming(&body, &actual_model, tmux).await
            } else {
                let json = crate::codex_cli::codex_cli_chat(&body, &actual_model, tmux).await?;
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
    let (provider_name, actual_model) = resolve_model(&model_str, &state);

    let pool = match provider_name.as_deref() {
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

/// GET /v1/models — list all available models across enabled providers.
///
/// Returns models declared in each provider's `models` config field.
/// For OpenAI-compatible providers with empty `models`, attempts to fetch
/// from upstream `{base_url}/models`. Model IDs use `provider:model` format.
#[instrument(skip(state))]
pub async fn list_models(State(state): State<Arc<AppState>>) -> Result<Json<Value>, GatewayError> {
    let pools = state.all_enabled_pools().await;
    let mut all_models: Vec<Value> = Vec::new();

    for pool in &pools {
        let provider_name = pool.name();
        let provider_type = pool.provider_type();
        let mut model_ids: Vec<String> = pool.config.models.clone();

        // For OpenAI-compatible providers with no declared models,
        // try fetching from upstream
        if model_ids.is_empty()
            && *provider_type == ProviderType::OpenAiCompatible
            && let Ok(fetched) = fetch_upstream_models(&state, pool).await
        {
            model_ids = fetched;
        }

        // For CLI providers, check PTY discovery cache
        if model_ids.is_empty()
            && *provider_type == ProviderType::CodexCli
            && let Some(cached) = state
                .get_cached_models(provider_name, std::time::Duration::from_secs(3600))
                .await
        {
            model_ids = cached;
        }

        // If still empty, add a "default" fallback
        if model_ids.is_empty() {
            model_ids.push("default".to_string());
        }

        let type_str = match provider_type {
            ProviderType::OpenAiCompatible => "open_ai_compatible",
            ProviderType::Anthropic => "anthropic",
            ProviderType::Cursor => "cursor",
            ProviderType::ClaudeCode => "claude_code",
            ProviderType::CodexCli => "codex_cli",
        };

        for model_id in &model_ids {
            all_models.push(json!({
                "id": format!("{provider_name}:{model_id}"),
                "object": "model",
                "owned_by": provider_name,
                "provider": provider_name,
                "provider_type": type_str,
            }));
        }
    }

    Ok(Json(json!({
        "object": "list",
        "data": all_models,
    })))
}

/// GET /v1/models/:provider — list models for a specific provider.
#[instrument(skip(state))]
pub async fn list_provider_models(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(provider): axum::extract::Path<String>,
) -> Result<Json<Value>, GatewayError> {
    let pool = state.get_pool(&provider).await?;
    let provider_type = pool.provider_type();
    let mut model_ids: Vec<String> = pool.config.models.clone();

    if model_ids.is_empty()
        && *provider_type == ProviderType::OpenAiCompatible
        && let Ok(fetched) = fetch_upstream_models(&state, &pool).await
    {
        model_ids = fetched;
    }

    // For CLI providers, check PTY discovery cache
    if model_ids.is_empty()
        && *provider_type == ProviderType::CodexCli
        && let Some(cached) = state
            .get_cached_models(&provider, std::time::Duration::from_secs(3600))
            .await
    {
        model_ids = cached;
    }

    if model_ids.is_empty() {
        model_ids.push("default".to_string());
    }

    let type_str = match provider_type {
        ProviderType::OpenAiCompatible => "open_ai_compatible",
        ProviderType::Anthropic => "anthropic",
        ProviderType::Cursor => "cursor",
        ProviderType::ClaudeCode => "claude_code",
        ProviderType::CodexCli => "codex_cli",
    };

    let models: Vec<Value> = model_ids
        .iter()
        .map(|model_id| {
            json!({
                "id": format!("{provider}:{model_id}"),
                "object": "model",
                "owned_by": &provider,
                "provider": &provider,
                "provider_type": type_str,
            })
        })
        .collect();

    Ok(Json(json!({
        "object": "list",
        "data": models,
    })))
}

/// Attempt to fetch model list from an OpenAI-compatible provider's /models endpoint.
async fn fetch_upstream_models(
    state: &AppState,
    pool: &crate::state::ProviderPool,
) -> Result<Vec<String>, GatewayError> {
    let url = format!("{}/models", pool.config.base_url.trim_end_matches('/'));
    let mut req = state.http_client.get(&url);
    if let Some(token) = pool.token_pool.next_token() {
        req = req.bearer_auth(token);
    }
    for (k, v) in &pool.config.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), req.send())
        .await
        .map_err(|_| GatewayError::UpstreamError("upstream models fetch timed out".into()))?
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    let json: Value = resp
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    let models = json["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(models)
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

/// Resolve a model string, supporting `hint:<name>` routing.
///
/// Resolution order:
/// 1. `"hint:<name>"` → look up in routing.model_routes → `(Some(provider), model)`
/// 2. `"provider:model"` → `(Some(provider), model)`
/// 3. `"model"` → check routing.default_route, then `(None, model)`
fn resolve_model(model_str: &str, state: &AppState) -> (Option<String>, String) {
    // Check for hint: prefix
    if let Some(hint) = model_str.strip_prefix("hint:")
        && let Some(ref routing) = state.routing
        && let Some(route) = routing.model_routes.get(hint)
    {
        let (p, m) = parse_provider_model(route);
        return (p.map(String::from), m.to_string());
    }

    // Standard provider:model parsing
    let (p, m) = parse_provider_model(model_str);
    if p.is_some() {
        return (p.map(|s| s.to_string()), m.to_string());
    }

    // No provider specified — check default route
    if let Some(ref routing) = state.routing
        && let Some(ref default_route) = routing.default_route
    {
        let (dp, _dm) = parse_provider_model(default_route);
        // Use default route's provider but keep the original model name
        if !model_str.is_empty() {
            return (dp.map(|s| s.to_string()), model_str.to_string());
        }
        return (dp.map(|s| s.to_string()), _dm.to_string());
    }

    (None, model_str.to_string())
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
