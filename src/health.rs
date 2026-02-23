use crate::state::AppState;
use axum::{extract::State, http::StatusCode, Json};
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
    pub enabled: bool,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
}

/// GET /health — checks all configured providers and returns overall status.
#[instrument(skip(state))]
pub async fn health_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<HealthResponse>) {
    let config = state.config.read().await;
    let client = state.http_client.as_ref();

    let mut provider_healths = Vec::with_capacity(config.providers.len());

    for provider in &config.providers {
        let health = check_provider(client, &provider.name, &provider.base_url).await;
        provider_healths.push(health);
    }

    let all_reachable = provider_healths
        .iter()
        .filter(|p| p.enabled)
        .all(|p| p.reachable);

    let status = if all_reachable { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };

    (
        status,
        Json(HealthResponse {
            status: if all_reachable { "ok" } else { "degraded" },
            providers: provider_healths,
        }),
    )
}

async fn check_provider(client: &Client, name: &str, base_url: &str) -> ProviderHealth {
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
            enabled: true,
            reachable: true,
            latency_ms: Some(start.elapsed().as_millis() as u64),
        },
        Err(_) => ProviderHealth {
            name: name.to_string(),
            enabled: true,
            reachable: false,
            latency_ms: None,
        },
    }
}
