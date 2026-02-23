mod error;
mod health;
mod middleware;
mod routes;
mod state;

use anyhow::{Context, Result};
use axum::{middleware::from_fn, routing::get, Router};
use evo_common::{config::GatewayConfig, logging::init_logging};
use state::AppState;
use std::{net::SocketAddr, path::Path, sync::Arc};
use tracing::info;

const DEFAULT_CONFIG_PATH: &str = "gateway.config";

#[tokio::main]
async fn main() -> Result<()> {
    // Init structured logging — guard must stay alive for the process lifetime
    let _log_guard = init_logging("gateway");

    let config_path = std::env::var("GATEWAY_CONFIG")
        .unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

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

    axum::serve(listener, app)
        .await
        .context("Server error")?;

    Ok(())
}

fn load_config(path: &str) -> Result<GatewayConfig> {
    if !Path::new(path).exists() {
        // Write a default config if none exists
        let default = default_config();
        let toml = default.to_toml()?;
        std::fs::write(path, &toml)
            .with_context(|| format!("Failed to write default config to {path}"))?;
        info!(path = %path, "wrote default gateway config");
        return Ok(default);
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {path}"))?;

    GatewayConfig::from_toml(&content)
        .with_context(|| format!("Failed to parse config from {path}"))
}

fn default_config() -> GatewayConfig {
    use evo_common::config::{ProviderConfig, ServerConfig};

    GatewayConfig {
        server: ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 8080,
        },
        providers: vec![
            ProviderConfig {
                name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_env: "OPENAI_API_KEY".to_string(),
                enabled: true,
                rate_limit: None,
            },
            ProviderConfig {
                name: "anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                enabled: true,
                rate_limit: None,
            },
            ProviderConfig {
                name: "ollama".to_string(),
                base_url: "http://localhost:11434".to_string(),
                api_key_env: "".to_string(),
                enabled: false,
                rate_limit: None,
            },
        ],
    }
}
