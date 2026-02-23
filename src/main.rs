mod error;
mod health;
mod middleware;
mod routes;
mod state;

use anyhow::{Context, Result};
use axum::{Router, middleware::from_fn, routing::get};
use evo_common::{config::GatewayConfig, logging::init_logging};
use state::AppState;
use std::{net::SocketAddr, path::Path, sync::Arc};
use tracing::info;

const DEFAULT_CONFIG_PATH: &str = "gateway.json";

#[tokio::main]
async fn main() -> Result<()> {
    // Init structured logging — guard must stay alive for the process lifetime
    let _log_guard = init_logging("gateway");

    let config_path =
        std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

    info!(config_path = %config_path, "loading gateway config");

    let config = load_config(&config_path)
        .with_context(|| format!("Failed to load config from {config_path}"))?;

    let host = config.server.host.clone();
    let port = config.server.port;

    let state = Arc::new(AppState::new(config));

    // Build router — state applied once at the top level
    let app = Router::new()
        .route("/health", get(health::health_handler))
        .merge(routes::router())
        .layer(from_fn(middleware::request_logging))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("Invalid bind address: {host}:{port}"))?;

    info!(addr = %addr, "evo-gateway listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind to {addr}"))?;

    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}

fn load_config(path: &str) -> Result<GatewayConfig> {
    if !Path::new(path).exists() {
        // Write a default config if none exists
        let default = default_config();
        let json = default
            .to_json()
            .context("Failed to serialize default config")?;
        std::fs::write(path, &json)
            .with_context(|| format!("Failed to write default config to {path}"))?;
        info!(path = %path, "wrote default gateway config");
        return Ok(default);
    }

    let content =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read {path}"))?;

    GatewayConfig::from_json(&content)
        .with_context(|| format!("Failed to parse JSON config from {path}"))
}

fn default_config() -> GatewayConfig {
    use evo_common::config::{ProviderConfig, ProviderType, ServerConfig};
    use std::collections::HashMap;

    GatewayConfig {
        server: ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 8080,
        },
        providers: vec![
            ProviderConfig {
                name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_envs: vec!["OPENAI_API_KEY".to_string()],
                enabled: true,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
            },
            ProviderConfig {
                name: "anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1".to_string(),
                api_key_envs: vec!["ANTHROPIC_API_KEY".to_string()],
                enabled: true,
                provider_type: ProviderType::Anthropic,
                extra_headers: HashMap::new(),
                rate_limit: None,
            },
            ProviderConfig {
                name: "openrouter".to_string(),
                base_url: "https://openrouter.ai/api/v1".to_string(),
                api_key_envs: vec!["OPENROUTER_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::from([
                    (
                        "HTTP-Referer".to_string(),
                        "https://github.com/ai-evo-agents".to_string(),
                    ),
                    ("X-Title".to_string(), "evo-gateway".to_string()),
                ]),
                rate_limit: None,
            },
            ProviderConfig {
                name: "ollama".to_string(),
                base_url: "http://localhost:11434".to_string(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
            },
        ],
    }
}
