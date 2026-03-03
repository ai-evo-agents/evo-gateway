//! GitHub Copilot provider.
//!
//! Exchanges a GitHub personal access token for a short-lived Copilot API token
//! via `https://api.github.com/copilot_internal/v2/token`, then proxies requests
//! using OpenAI-compatible wire format with the dynamic base URL from the token.

use crate::error::GatewayError;
use crate::state::{AppState, ProviderPool};
use crate::stream::{is_streaming, proxy_streaming};
use axum::Json;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// ─── Constants ──────────────────────────────────────────────────────────────

const TOKEN_ENDPOINT: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
/// Refresh token when it expires within this buffer.
const EXPIRY_BUFFER: Duration = Duration::from_secs(300);

// ─── Token management ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct CachedToken {
    token: String,
    base_url: String,
    expires_at: Instant,
}

pub struct CopilotAuth {
    cache: RwLock<Option<CachedToken>>,
}

impl CopilotAuth {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(None),
        }
    }

    /// Get a valid Copilot token, refreshing if expired or about to expire.
    pub async fn get_token(
        &self,
        client: &reqwest::Client,
        pool: &ProviderPool,
    ) -> Result<(String, String), GatewayError> {
        // Check cache first
        {
            let cached = self.cache.read().await;
            if let Some(ref t) = *cached
                && t.expires_at > Instant::now() + EXPIRY_BUFFER
            {
                return Ok((t.token.clone(), t.base_url.clone()));
            }
        }

        // Need to refresh
        let github_token = resolve_github_token(pool)?;
        let new_token = exchange_token(client, &github_token).await?;

        let result = (new_token.token.clone(), new_token.base_url.clone());
        let mut cache = self.cache.write().await;
        *cache = Some(new_token);
        Ok(result)
    }
}

/// Resolve GitHub token from pool's api_key_envs or well-known env vars.
fn resolve_github_token(pool: &ProviderPool) -> Result<String, GatewayError> {
    // Try pool's token pool first
    if let Some(t) = pool.token_pool.next_token() {
        return Ok(t.to_string());
    }

    // Fallback to well-known env vars
    for var in &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.is_empty()
        {
            return Ok(t);
        }
    }

    Err(GatewayError::ConfigError(
        "no GitHub token available for Copilot provider — set COPILOT_GITHUB_TOKEN, GH_TOKEN, \
         or GITHUB_TOKEN"
            .into(),
    ))
}

/// Exchange a GitHub personal access token for a short-lived Copilot API token.
async fn exchange_token(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<CachedToken, GatewayError> {
    let resp = client
        .get(TOKEN_ENDPOINT)
        .header("Authorization", format!("token {github_token}"))
        .header("User-Agent", "evo-gateway")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| GatewayError::UpstreamError(format!("Copilot token exchange failed: {e}")))?;

    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| {
        GatewayError::UpstreamError(format!("Copilot token response parse error: {e}"))
    })?;

    if !status.is_success() {
        return Err(GatewayError::Unauthorized(format!(
            "GitHub Copilot token exchange returned {status}: {body}"
        )));
    }

    let token = body["token"]
        .as_str()
        .ok_or_else(|| {
            GatewayError::UpstreamError("Copilot token response missing 'token' field".into())
        })?
        .to_string();

    // Parse expiry — may be seconds or milliseconds since epoch
    let expires_at_raw = body["expires_at"].as_i64().unwrap_or(0);
    let expires_at_secs = if expires_at_raw > 1_000_000_000_000 {
        // Milliseconds
        expires_at_raw / 1000
    } else {
        expires_at_raw
    };
    let now_secs = chrono::Utc::now().timestamp();
    let ttl_secs = (expires_at_secs - now_secs).max(60) as u64;

    // Extract API base URL from response
    let base_url = body["endpoints"]["api"]
        .as_str()
        .unwrap_or(DEFAULT_BASE_URL)
        .to_string();

    Ok(CachedToken {
        token,
        base_url,
        expires_at: Instant::now() + Duration::from_secs(ttl_secs),
    })
}

// ─── Chat handler ───────────────────────────────────────────────────────────

/// Handle a chat completion request via GitHub Copilot.
///
/// Uses the cached Copilot token and dynamic base URL. The wire format is
/// standard OpenAI chat/completions.
pub async fn copilot_chat(
    state: &AppState,
    pool: &ProviderPool,
    copilot_auth: &Arc<CopilotAuth>,
    body: Value,
) -> Result<Response, GatewayError> {
    let (token, base_url) = copilot_auth.get_token(&state.http_client, pool).await?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let req = state
        .http_client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("Copilot-Integration-Id", "vscode-chat")
        .json(&body);

    if is_streaming(&body) {
        proxy_streaming(req).await
    } else {
        let response = req.send().await.map_err(GatewayError::from)?;
        let status = response.status();
        let json: Value = response
            .json()
            .await
            .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

        if !status.is_success() {
            return Err(GatewayError::UpstreamError(format!(
                "GitHub Copilot returned {status}: {json}"
            )));
        }
        Ok(Json(json).into_response())
    }
}
