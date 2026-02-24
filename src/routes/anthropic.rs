use crate::{
    error::GatewayError,
    state::AppState,
    stream::{is_streaming, proxy_streaming},
};
use axum::{Json, extract::State, response::IntoResponse, response::Response};
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

/// POST /v1/messages — proxies directly to the Anthropic Messages API (native format).
///
/// This endpoint accepts Anthropic's native request schema without translation.
/// Supports streaming (`"stream": true`) — returns SSE events without buffering.
/// Use `/v1/chat/completions` with `"anthropic:<model>"` for OpenAI-compatible access.
#[instrument(skip(state, body))]
pub async fn messages(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Response, GatewayError> {
    let pool = state.get_pool("anthropic").await?;

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
        let response = req.json(&body).send().await.map_err(GatewayError::from)?;

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

        Ok(Json(json).into_response())
    }
}
