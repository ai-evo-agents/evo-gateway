use crate::{error::GatewayError, state::AppState};
use axum::{extract::State, Json};
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

/// POST /api/generate — Ollama-compatible generate endpoint.
#[instrument(skip(state, body))]
pub async fn generate(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let provider = state.get_provider("ollama").await.or_else(|_| {
        // Fall back to any local LLM provider
        futures_util_unavailable()
    })?;

    let url = format!("{}/api/generate", provider.base_url);

    let response = state
        .http_client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(GatewayError::from)?;

    let status = response.status();
    let json: Value = response
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    if !status.is_success() {
        return Err(GatewayError::UpstreamError(format!(
            "Local LLM returned {status}: {json}"
        )));
    }

    Ok(Json(json))
}

/// POST /api/chat — Ollama-compatible chat endpoint.
#[instrument(skip(state, body))]
pub async fn chat(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let provider = state
        .get_preferred_provider(&["ollama", "local"])
        .await?;

    let url = format!("{}/api/chat", provider.base_url);

    let response = state
        .http_client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(GatewayError::from)?;

    let status = response.status();
    let json: Value = response
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    if !status.is_success() {
        return Err(GatewayError::UpstreamError(format!(
            "Local LLM returned {status}: {json}"
        )));
    }

    Ok(Json(json))
}

/// Helper: no local LLM provider available.
fn futures_util_unavailable() -> Result<evo_common::config::ProviderConfig, GatewayError> {
    Err(GatewayError::ProviderNotFound(
        "ollama or local".to_string(),
    ))
}
