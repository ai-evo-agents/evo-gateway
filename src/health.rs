use crate::state::AppState;
use axum::{Json, extract::State, http::StatusCode};
use evo_common::config::ProviderType;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::instrument;

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub providers: Vec<ProviderHealth>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub name: String,
    pub tokens: usize,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
}

/// GET /health — probes all enabled provider base URLs and reports status.
#[instrument(skip(state))]
pub async fn health_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<HealthResponse>) {
    let pools = state.all_enabled_pools().await;
    let client = state.http_client.as_ref();

    let mut provider_healths = Vec::with_capacity(pools.len());

    for pool in &pools {
        let health = match pool.provider_type() {
            ProviderType::Cursor => check_cursor_provider(pool.name()).await,
            _ => {
                check_provider(
                    client,
                    pool.name(),
                    &pool.config.base_url,
                    pool.token_pool.len(),
                )
                .await
            }
        };
        provider_healths.push(health);
    }

    let all_reachable = provider_healths.iter().all(|p| p.reachable);
    let status = if all_reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(HealthResponse {
            status: if all_reachable { "ok" } else { "degraded" },
            providers: provider_healths,
        }),
    )
}

/// Health check for the Cursor provider — runs `cursor-agent status` subprocess.
async fn check_cursor_provider(name: &str) -> ProviderHealth {
    let start = std::time::Instant::now();
    let (reachable, _email) = crate::cursor::check_cursor_status().await;
    ProviderHealth {
        name: name.to_string(),
        tokens: 0,
        reachable,
        latency_ms: Some(start.elapsed().as_millis() as u64),
    }
}

async fn check_provider(
    client: &Client,
    name: &str,
    base_url: &str,
    tokens: usize,
) -> ProviderHealth {
    let health_url = format!("{base_url}/");
    let start = std::time::Instant::now();

    match client
        .get(&health_url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(_) => ProviderHealth {
            name: name.to_string(),
            tokens,
            reachable: true,
            latency_ms: Some(start.elapsed().as_millis() as u64),
        },
        Err(_) => ProviderHealth {
            name: name.to_string(),
            tokens,
            reachable: false,
            latency_ms: None,
        },
    }
}
