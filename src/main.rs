mod claude_code;
mod cli_common;
mod codex_auth;
mod codex_cli;
mod cursor;
mod db;
mod error;
mod health;
mod middleware;
mod reliability;
mod routes;
mod service;
mod session_socketio;
mod state;
mod stream;
mod tmux;

use anyhow::{Context, Result};
use axum::{
    Router,
    middleware::{from_fn, from_fn_with_state},
    routing::get,
};
use clap::{Parser, Subcommand};
use evo_common::{config::GatewayConfig, logging::init_logging_with_otel};
use evo_gateway::auth::AuthStore;
use middleware::auth::{AuthState, auth_middleware};
use socketioxide::SocketIo;
use state::AppState;
use std::{net::SocketAddr, path::Path, sync::Arc};
use tmux::TmuxManager;
use tokio::sync::RwLock;
use tracing::info;

const DEFAULT_CONFIG_PATH: &str = "gateway.json";

#[derive(Parser)]
#[command(
    name = "evo-gateway",
    version,
    about = "Unified LLM API gateway for the evo system"
)]
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
    /// Manage evo-gateway as a system service (launchd on macOS, systemd on Linux)
    Service {
        #[command(subcommand)]
        action: service::ServiceAction,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Authenticate with Cursor via browser OAuth
    Cursor,
    /// Authenticate with Claude Code CLI
    Claude,
    /// Authenticate with Codex CLI
    Codex,
    /// Authenticate with OpenAI Codex Responses API (access token)
    CodexAuth {
        /// API key or OAuth bearer token (skip interactive prompt)
        #[arg(long)]
        token: Option<String>,
        /// ChatGPT account ID to send with requests (optional)
        #[arg(long)]
        account_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Init structured logging with OpenTelemetry — guards must stay alive
    let otlp_endpoint =
        std::env::var("EVO_OTLP_ENDPOINT").unwrap_or_else(|_| "http://localhost:3300".to_string());
    let (_log_guard, _otel_guard) = init_logging_with_otel("gateway", &otlp_endpoint);

    // Handle CLI subcommands
    match cli.command {
        Some(Commands::Auth { action }) => {
            return match action {
                AuthCommands::Cursor => run_cursor_auth().await,
                AuthCommands::Claude => run_claude_auth().await,
                AuthCommands::Codex => run_codex_auth().await,
                AuthCommands::CodexAuth { token, account_id } => {
                    run_codex_auth_flow(token, account_id).await
                }
            };
        }
        Some(Commands::Service { action }) => {
            return service::run_service_command(action).await;
        }
        None => {} // Continue to start server
    }

    // ── Start server (default, no subcommand) ───────────────────────────

    let config_path =
        std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

    info!(config_path = %config_path, "loading gateway config");

    let config = load_config(&config_path)
        .with_context(|| format!("Failed to load config from {config_path}"))?;

    let host = config.server.host.clone();
    let port = config.server.port;

    // ── Tmux session manager ──────────────────────────────────────────────────
    let tmux_mode = std::env::var("EVO_GATEWAY_TMUX").unwrap_or_else(|_| "auto".into());
    let tmux_manager = match tmux_mode.as_str() {
        "disabled" => {
            info!("tmux integration disabled by config");
            None
        }
        "enabled" => {
            let version = TmuxManager::check_tmux_available()
                .await
                .context("tmux is required (EVO_GATEWAY_TMUX=enabled) but not found")?;
            info!(version = %version, "tmux required and available");
            let output_dir = std::env::var("EVO_GATEWAY_TMUX_OUTPUT_DIR")
                .unwrap_or_else(|_| "./tmux-output".into())
                .into();
            let max_sessions = std::env::var("EVO_GATEWAY_MAX_TMUX_SESSIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8);
            let mgr = Arc::new(TmuxManager::new(output_dir, max_sessions));
            mgr.cleanup_orphaned().await;
            Some(mgr)
        }
        _ => {
            // "auto" mode — use tmux if available
            match TmuxManager::check_tmux_available().await {
                Ok(version) => {
                    info!(version = %version, "tmux available, enabling session management");
                    let output_dir = std::env::var("EVO_GATEWAY_TMUX_OUTPUT_DIR")
                        .unwrap_or_else(|_| "./tmux-output".into())
                        .into();
                    let max_sessions = std::env::var("EVO_GATEWAY_MAX_TMUX_SESSIONS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(8);
                    let mgr = Arc::new(TmuxManager::new(output_dir, max_sessions));
                    mgr.cleanup_orphaned().await;
                    Some(mgr)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "tmux not available, falling back to direct subprocess");
                    None
                }
            }
        }
    };

    // ── Socket.IO server ──────────────────────────────────────────────────────
    let socketio_enabled = std::env::var("EVO_GATEWAY_SOCKETIO")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);

    let (socket_layer, io) = if socketio_enabled {
        let (layer, io) = SocketIo::new_layer();
        info!("Socket.IO server enabled for session management");
        (Some(layer), Some(io))
    } else {
        info!("Socket.IO server disabled");
        (None, None)
    };

    let state = Arc::new(AppState::new(config, tmux_manager.clone(), io.clone()));

    // Register Socket.IO handlers
    if let Some(io) = &io {
        session_socketio::register_handlers(io.clone(), Arc::clone(&state));
    }

    // Spawn tmux cleanup task
    if let Some(ref mgr) = tmux_manager {
        let mgr = Arc::clone(mgr);
        tokio::spawn(async move {
            tmux::cleanup_loop(mgr).await;
        });
    }

    // Check cursor auth status on startup
    check_cursor_auth_status().await;

    // Check claude code availability on startup
    check_claude_code_availability().await;
    check_provider_auth_status("claude-code").await;

    // Check codex CLI availability on startup
    check_codex_cli_availability().await;
    check_provider_auth_status("codex-cli").await;

    // Background PTY model discovery for CLI providers
    {
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            discover_cli_models_background(state_clone).await;
        });
    }

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
    let mut app = Router::new()
        .route("/health", get(health::health_handler))
        .merge(routes::router().layer(from_fn_with_state(auth_state, auth_middleware)))
        .layer(from_fn(middleware::request_logging));

    // Add Socket.IO layer if enabled (intercepts /socket.io/ path)
    if let Some(layer) = socket_layer {
        app = app.layer(layer);
    }

    let app = app.with_state(state);

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

/// Interactive Claude Code CLI authentication flow.
///
/// Runs `claude` with inherited stdio so the user can complete auth interactively.
async fn run_claude_auth() -> Result<()> {
    use std::process::Stdio;

    let binary = std::env::var("CLAUDE_CODE_BINARY").unwrap_or_else(|_| "claude".into());

    println!("Starting Claude Code authentication...");
    println!("This will run `{binary}` — complete the auth flow in the terminal.\n");

    let status = tokio::process::Command::new(&binary)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("Failed to run '{binary}'. Is Claude Code CLI installed?"))?;

    if !status.success() {
        anyhow::bail!("{binary} exited with code: {status}");
    }

    // Capture version after successful run
    let output = tokio::process::Command::new(&binary)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("Failed to run '{binary} --version'"))?;

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let db = crate::db::init_db()
        .await
        .context("Failed to initialize database")?;
    let conn = db.connect().context("Failed to connect to database")?;
    crate::db::save_provider_auth(&conn, "claude-code", &version)
        .await
        .context("Failed to save claude auth")?;

    println!("\nClaude Code authenticated successfully!");
    if !version.is_empty() {
        println!("  Version: {version}");
    }
    println!("\nEnable claude-code in your gateway.json by setting claude-code.enabled = true");

    Ok(())
}

/// Interactive Codex CLI authentication flow.
async fn run_codex_auth() -> Result<()> {
    use std::process::Stdio;

    let binary = std::env::var("CODEX_CLI_BINARY").unwrap_or_else(|_| "codex".into());

    println!("Starting Codex CLI authentication...");
    println!("This will run `{binary}` — complete the auth flow in the terminal.\n");

    let status = tokio::process::Command::new(&binary)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("Failed to run '{binary}'. Is Codex CLI installed?"))?;

    if !status.success() {
        anyhow::bail!("{binary} exited with code: {status}");
    }

    let output = tokio::process::Command::new(&binary)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("Failed to run '{binary} --version'"))?;

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let db = crate::db::init_db()
        .await
        .context("Failed to initialize database")?;
    let conn = db.connect().context("Failed to connect to database")?;
    crate::db::save_provider_auth(&conn, "codex-cli", &version)
        .await
        .context("Failed to save codex auth")?;

    println!("\nCodex CLI authenticated successfully!");
    if !version.is_empty() {
        println!("  Version: {version}");
    }
    println!("\nEnable codex-cli in your gateway.json by setting codex-cli.enabled = true");

    Ok(())
}

/// Interactive authentication flow for OpenAI Codex Responses API.
///
/// Accepts optional pre-supplied `token` / `account_id` (from `--token` / `--account-id`
/// flags) to allow non-interactive use. Falls back to interactive stdin prompts.
///
/// Stores credentials in the gateway database. The DB path is resolved the same way
/// the server resolves it: `EVO_GATEWAY_DB_PATH` env or `gateway.db` relative to CWD.
async fn run_codex_auth_flow(
    cli_token: Option<String>,
    cli_account_id: Option<String>,
) -> Result<()> {
    use std::io::{self, BufRead, Write};

    println!("OpenAI Codex Responses API Authentication");
    println!("==========================================\n");

    // ── Token ──────────────────────────────────────────────────────────────
    let token = if let Some(t) = cli_token {
        // Non-interactive: token supplied via --token flag
        println!("Using token from --token flag.");
        t
    } else {
        println!("Paste your OpenAI API key or access token below.");
        println!("(This will be stored in the gateway database for API requests.)");
        println!("Tip: you can also run non-interactively with --token <TOKEN>\n");

        print!("Access token: ");
        io::stdout().flush()?;
        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let t = line.trim().to_string();
        if t.is_empty() {
            anyhow::bail!("No token provided. Aborting.");
        }
        t
    };

    // ── Account ID ─────────────────────────────────────────────────────────
    let account_id = if let Some(id) = cli_account_id {
        if id.is_empty() {
            None
        } else {
            Some(id)
        }
    } else {
        print!("Account ID (optional, press Enter to skip): ");
        io::stdout().flush()?;
        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    };

    let db = crate::db::init_db()
        .await
        .context("Failed to initialize database")?;
    let conn = db.connect().context("Failed to connect to database")?;
    crate::db::save_codex_auth_token(&conn, &token, account_id.as_deref())
        .await
        .context("Failed to save codex-auth token")?;

    println!("\nCodex Auth configured successfully!");
    println!(
        "  Token: {}...{}",
        &token[..8.min(token.len())],
        if token.len() > 12 {
            &token[token.len() - 4..]
        } else {
            ""
        }
    );
    if let Some(ref id) = account_id {
        println!("  Account ID: {id}");
    }
    println!("\nEnable codex-auth in your gateway.json by setting codex-auth.enabled = true");

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

/// Check and log Codex CLI availability on server startup.
async fn check_codex_cli_availability() {
    let (available, version) = crate::codex_cli::check_codex_cli_status().await;
    if available {
        info!(
            version = ?version,
            "codex CLI available"
        );
    } else {
        info!("codex CLI not found — codex-cli provider will not work until `codex` is installed");
    }
}

/// Check and log a provider's auth status on server startup.
async fn check_provider_auth_status(provider: &str) {
    match crate::db::init_db().await {
        Ok(db) => {
            if let Ok(conn) = db.connect() {
                match crate::db::get_provider_auth(&conn, provider).await {
                    Ok(Some(auth)) => {
                        info!(
                            provider = %provider,
                            status = %auth.status,
                            "provider auth status"
                        );
                    }
                    Ok(None) => {
                        info!(
                            provider = %provider,
                            "provider not authenticated — run `evo-gateway auth {provider}` to set up"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(provider = %provider, error = %e, "failed to check provider auth");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                provider = %provider,
                error = %e,
                "failed to init gateway database (auth check skipped)"
            );
        }
    }
}

/// Background task: discover models for CLI providers via PTY.
///
/// Runs after startup to populate the models cache without blocking the server.
/// Only discovers models for providers that have empty `models` config.
async fn discover_cli_models_background(state: Arc<AppState>) {
    use evo_common::config::ProviderType;

    let pools = state.all_enabled_pools().await;

    for pool in &pools {
        if *pool.provider_type() == ProviderType::CodexCli && pool.config.models.is_empty() {
            info!(provider = %pool.name(), "starting PTY model discovery for codex-cli");
            match crate::codex_cli::discover_codex_models().await {
                Ok(models) => {
                    info!(
                        provider = %pool.name(),
                        count = models.len(),
                        models = ?models,
                        "discovered codex-cli models via PTY"
                    );
                    state.set_cached_models(pool.name(), models).await;
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %pool.name(),
                        error = %e,
                        "PTY model discovery failed — using config-defined models as fallback"
                    );
                }
            }
        }
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
                models: vec![
                    "gpt-4o".into(),
                    "gpt-4o-mini".into(),
                    "gpt-4.1".into(),
                    "gpt-4.1-mini".into(),
                    "gpt-4.1-nano".into(),
                    "o3".into(),
                    "o3-mini".into(),
                    "o4-mini".into(),
                ],
            },
            ProviderConfig {
                name: "anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1".to_string(),
                api_key_envs: vec!["ANTHROPIC_API_KEY".to_string()],
                enabled: true,
                provider_type: ProviderType::Anthropic,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "claude-opus-4-5".into(),
                    "claude-sonnet-4-5".into(),
                    "claude-haiku-3-5".into(),
                ],
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
                models: vec![],
            },
            ProviderConfig {
                name: "gemini".to_string(),
                base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
                api_key_envs: vec!["GEMINI_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "gemini-2.5-pro".into(),
                    "gemini-2.5-flash".into(),
                    "gemini-2.0-flash".into(),
                ],
            },
            ProviderConfig {
                name: "deepseek".to_string(),
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key_envs: vec!["DEEPSEEK_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
            },
            ProviderConfig {
                name: "mistral".to_string(),
                base_url: "https://api.mistral.ai/v1".to_string(),
                api_key_envs: vec!["MISTRAL_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec!["mistral-large-latest".into(), "mistral-small-latest".into()],
            },
            ProviderConfig {
                name: "groq".to_string(),
                base_url: "https://api.groq.com/openai/v1".to_string(),
                api_key_envs: vec!["GROQ_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "llama-3.3-70b-versatile".into(),
                    "mixtral-8x7b-32768".into(),
                ],
            },
            ProviderConfig {
                name: "together".to_string(),
                base_url: "https://api.together.xyz/v1".to_string(),
                api_key_envs: vec!["TOGETHER_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![],
            },
            ProviderConfig {
                name: "xai".to_string(),
                base_url: "https://api.x.ai/v1".to_string(),
                api_key_envs: vec!["XAI_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec!["grok-3".into(), "grok-3-mini".into()],
            },
            ProviderConfig {
                name: "ollama".to_string(),
                base_url: "http://localhost:11434".to_string(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::OpenAiCompatible,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![],
            },
            ProviderConfig {
                name: "cursor".to_string(),
                base_url: String::new(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::Cursor,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "auto".into(),
                    "gpt-4o".into(),
                    "claude-sonnet-4-5".into(),
                    "claude-opus-4-5".into(),
                ],
            },
            ProviderConfig {
                name: "claude-code".to_string(),
                base_url: String::new(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::ClaudeCode,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "claude-sonnet-4-5".into(),
                    "claude-opus-4-5".into(),
                    "claude-haiku-3-5".into(),
                ],
            },
            ProviderConfig {
                name: "codex-cli".to_string(),
                base_url: String::new(),
                api_key_envs: vec![],
                enabled: false,
                provider_type: ProviderType::CodexCli,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "gpt-5.3-codex".into(),
                    "gpt-5.2-codex".into(),
                    "gpt-5.1-codex-max".into(),
                    "gpt-5.2".into(),
                    "gpt-5.1-codex-mini".into(),
                    "default".into(),
                ],
            },
            ProviderConfig {
                name: "codex-auth".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_envs: vec!["OPENAI_API_KEY".to_string()],
                enabled: false,
                provider_type: ProviderType::CodexAuth,
                extra_headers: HashMap::new(),
                rate_limit: None,
                models: vec![
                    "codex-mini-latest".into(),
                    "gpt-4.1".into(),
                    "gpt-4.1-mini".into(),
                    "o3".into(),
                    "o4-mini".into(),
                ],
            },
        ],
        reliability: None,
        routing: None,
    }
}
