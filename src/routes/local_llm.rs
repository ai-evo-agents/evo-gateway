use crate::{error::GatewayError, state::AppState};
use axum::{Json, extract::State};
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

/// POST /api/generate — Ollama-compatible generate endpoint.
///
/// Routes to the first enabled local LLM provider (prefers `ollama`).
#[instrument(skip(state, body))]
pub async fn generate(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let pool = state.get_preferred_pool(&["ollama"]).await?;
    let url = format!("{}/api/generate", pool.config.base_url);

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
///
/// Routes to the first enabled local LLM provider (prefers `ollama`).
#[instrument(skip(state, body))]
pub async fn chat(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let pool = state.get_preferred_pool(&["ollama"]).await?;
    let url = format!("{}/api/chat", pool.config.base_url);

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
