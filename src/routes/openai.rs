use crate::{error::GatewayError, state::AppState};
use axum::{extract::State, Json};
use serde_json::{json, Value};
use std::{env, sync::Arc};
use tracing::instrument;

/// POST /v1/chat/completions — proxies to the best available OpenAI-compatible provider.
#[instrument(skip(state, body))]
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let provider = state
        .get_preferred_provider(&["openai", "anthropic", "ollama"])
        .await?;

    let api_key = resolve_api_key(&provider.api_key_env);
    let url = format!("{}/chat/completions", provider.base_url);

    let mut request = state.http_client.post(&url).json(&body);

    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    let response = request.send().await.map_err(GatewayError::from)?;
    let status = response.status();
    let json: Value = response
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    if !status.is_success() {
        return Err(GatewayError::UpstreamError(format!(
            "Provider returned {status}: {json}"
        )));
    }

    Ok(Json(json))
}

/// POST /v1/embeddings — proxies to an embeddings-capable provider.
#[instrument(skip(state, body))]
pub async fn embeddings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let provider = state.get_preferred_provider(&["openai", "ollama"]).await?;
    let api_key = resolve_api_key(&provider.api_key_env);
    let url = format!("{}/embeddings", provider.base_url);

    let mut request = state.http_client.post(&url).json(&body);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    let response = request.send().await.map_err(GatewayError::from)?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    Ok(Json(json))
}

/// GET /v1/models — returns list of available models from all enabled providers.
#[instrument(skip(state))]
pub async fn list_models(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, GatewayError> {
    let config = state.config.read().await;
    let enabled_providers: Vec<&str> = config
        .providers
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.name.as_str())
        .collect();

    Ok(Json(json!({
        "object": "list",
        "data": enabled_providers.iter().map(|name| json!({
            "id": name,
            "object": "model",
            "owned_by": "evo-gateway"
        })).collect::<Vec<_>>()
    })))
}

fn resolve_api_key(env_var: &str) -> Option<String> {
    if env_var.is_empty() {
        return None;
    }
    env::var(env_var).ok()
}
