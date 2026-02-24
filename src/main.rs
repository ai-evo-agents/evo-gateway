mod claude_code;
mod cli_common;
mod cursor;
mod db;
mod error;
mod health;
mod middleware;
mod routes;
mod state;
mod stream;

use anyhow::{Context, Result};
use axum::{
    Router,
    middleware::{from_fn, from_fn_with_state},
    routing::get,
};
use clap::{Parser, Subcommand};
use evo_common::{config::GatewayConfig, logging::init_logging};
use evo_gateway::auth::AuthStore;
use middleware::auth::{AuthState, auth_middleware};
use state::AppState;
use std::{net::SocketAddr, path::Path, sync::Arc};
use tokio::sync::RwLock;
use tracing::info;

const DEFAULT_CONFIG_PATH: &str = "gateway.json";

#[derive(Parser)]
#[command(name = "evo-gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate with a provider
    Auth {
        #[command(subcommand)]
        action: AuthCommands,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Authenticate with Cursor via browser OAuth
    Cursor,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Init structured logging — guard must stay alive for the process lifetime
    let _log_guard = init_logging("gateway");

    // Handle CLI subcommands
    if let Some(Commands::Auth { action }) = cli.command {
        return match action {
            AuthCommands::Cursor => run_cursor_auth().await,
        };
    }

    // ── Start server (default, no subcommand) ───────────────────────────

    let config_path =
        std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

    info!(config_path = %config_path, "loading gateway config");

    let config = load_config(&config_path)
        .with_context(|| format!("Failed to load config from {config_path}"))?;

    let host = config.server.host.clone();
    let port = config.server.port;

    let state = Arc::new(AppState::new(config));

    // Check cursor auth status on startup
    check_cursor_auth_status().await;

    // Check claude code availability on startup
    check_claude_code_availability().await;

    // Auth configuration
    let auth_enabled = std::env::var("EVO_GATEWAY_AUTH")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let auth_keys_path =
        std::env::var("EVO_GATEWAY_AUTH_KEYS").unwrap_or_else(|_| "auth.json".to_string());

    let auth_store = if auth_enabled {
        AuthStore::load(Path::new(&auth_keys_path))
            .with_context(|| format!("Failed to load auth keys from {auth_keys_path}"))?
    } else {
        AuthStore::default()
    };

    if auth_enabled {
        info!(
            keys_file = %auth_keys_path,
            keys_count = auth_store.keys.len(),
            "authentication enabled"
        );
    }

    let auth_state = AuthState {
        enabled: auth_enabled,
        store: Arc::new(RwLock::new(auth_store)),
    };

    // Build router — /health stays unauthenticated, API routes get auth middleware
    let app = Router::new()
        .route("/health", get(health::health_handler))
        .merge(routes::router().layer(from_fn_with_state(auth_state, auth_middleware)))
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

/// Interactive cursor-agent authentication flow.
async fn run_cursor_auth() -> Result<()> {
    use std::process::Stdio;

    let binary = std::env::var("CURSOR_AGENT_BINARY").unwrap_or_else(|_| "cursor-agent".into());

    println!("Starting Cursor authentication...");
    println!("This will open your browser for OAuth login.\n");

    // Step 1: Run `cursor-agent login` (interactive, inherits stdio)
    let status = tokio::process::Command::new(&binary)
        .arg("login")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("Failed to run '{binary} login'. Is cursor-agent installed?"))?;

    if !status.success() {
        anyhow::bail!("cursor-agent login failed with exit code: {status}");
    }

    println!("\nVerifying authentication...");

    // Step 2: Run `cursor-agent status` to capture email
    let output = tokio::process::Command::new(&binary)
        .arg("status")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("Failed to run '{binary} status'"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let email = stdout
        .lines()
        .find(|l| l.contains('@'))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    // Step 3: Save auth status to DB
    let db = crate::db::init_db()
        .await
        .context("Failed to initialize database")?;
    let conn = db.connect().context("Failed to connect to database")?;
    crate::db::save_cursor_auth(&conn, &email)
        .await
        .context("Failed to save cursor auth")?;

    println!("\nCursor authenticated successfully!");
    println!("  Email: {email}");
    println!("\nEnable cursor in your gateway.json by setting cursor.enabled = true");

    Ok(())
}

/// Check and log cursor auth status on server startup.
async fn check_cursor_auth_status() {
    match crate::db::init_db().await {
        Ok(db) => {
            if let Ok(conn) = db.connect() {
                match crate::db::get_cursor_auth(&conn).await {
                    Ok(Some(auth)) => {
                        info!(
                            status = %auth.status,
                            email = ?auth.email,
                            "cursor auth status"
                        );
                    }
                    Ok(None) => {
                        info!("cursor not authenticated — run `cargo run auth cursor` to set up");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to check cursor auth");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to init gateway database (cursor auth check skipped)");
        }
    }
}

/// Check and log Claude Code CLI availability on server startup.
async fn check_claude_code_availability() {
    let (available, version) = crate::claude_code::check_claude_code_status().await;
    if available {
        info!(
            version = ?version,
            "claude code CLI available"
        );
    } else {
        info!(
            "claude code CLI not found — claude-code provider will not work until `claude` is installed"
        );
    }
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
            ProviderConfig {
                name: "cursor".to_string(),
                base_url: String::new(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::Cursor,
                extra_headers: HashMap::new(),
                rate_limit: None,
            },
            ProviderConfig {
                name: "claude-code".to_string(),
                base_url: String::new(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::ClaudeCode,
                extra_headers: HashMap::new(),
                rate_limit: None,
            },
        ],
    }
}
