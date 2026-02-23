use crate::{error::GatewayError, state::AppState};
use axum::{extract::State, Json};
use serde_json::Value;
use std::{env, sync::Arc};
use tracing::instrument;

/// POST /v1/messages — proxies to the Anthropic messages API.
#[instrument(skip(state, body))]
pub async fn messages(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, GatewayError> {
    let provider = state.get_provider("anthropic").await?;
    let api_key = env::var(&provider.api_key_env)
        .map_err(|_| GatewayError::ConfigError(format!("{} env var not set", provider.api_key_env)))?;

    let url = format!("{}/messages", provider.base_url);

    let response = state
        .http_client
        .post(&url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
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
            "Anthropic returned {status}: {json}"
        )));
    }

    Ok(Json(json))
}
