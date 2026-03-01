use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database};
use tracing::info;

const DEFAULT_DB_PATH: &str = "gateway.db";

/// Absolute path used exclusively for codex-auth credential storage:
/// `$HOME/.evo-gateway/gateway.db`.
///
/// This makes `evo-gateway auth codex-auth` and the running server agree on
/// the same file regardless of their respective working directories.
pub fn codex_auth_db_path() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home)
        .join(".evo-gateway")
        .join("gateway.db")
}

/// Open (or create) the codex-auth credential database at the fixed home-dir
/// path returned by [`codex_auth_db_path`].
///
/// Creates `~/.evo-gateway/` if it does not exist yet.
pub async fn init_codex_auth_db() -> Result<Database> {
    let path = codex_auth_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let path_str = path
        .to_str()
        .context("codex-auth DB path is not valid UTF-8")?
        .to_string();

    let db = Builder::new_local(&path_str)
        .build()
        .await
        .with_context(|| format!("Failed to open codex-auth database at {path_str}"))?;

    let conn = db
        .connect()
        .context("Failed to connect to codex-auth database")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS credentials (
            provider   TEXT PRIMARY KEY,
            status     TEXT NOT NULL,
            email      TEXT,
            api_key    TEXT,
            updated_at TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("Failed to create credentials table in codex-auth DB")?;

    info!(path = %path_str, "codex-auth database initialized");
    Ok(db)
}

/// Credential record for any provider (cursor, claude-code, codex-cli, etc.).
#[derive(Debug, Clone)]
pub struct ProviderAuth {
    pub status: String,
    pub email: Option<String>,
}

/// Open (or create) the local libSQL database and ensure the schema exists.
pub async fn init_db() -> Result<Database> {
    let path = std::env::var("EVO_GATEWAY_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.into());
    let db = Builder::new_local(&path)
        .build()
        .await
        .with_context(|| format!("Failed to open database at {path}"))?;

    let conn = db
        .connect()
        .with_context(|| "Failed to connect to database")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS credentials (
            provider   TEXT PRIMARY KEY,
            status     TEXT NOT NULL,
            email      TEXT,
            api_key    TEXT,
            updated_at TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("Failed to create credentials table")?;

    info!(path = %path, "gateway database initialized");
    Ok(db)
}

/// Store (upsert) provider authentication status.
pub async fn save_provider_auth(conn: &Connection, provider: &str, email: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO credentials (provider, status, email, updated_at)
         VALUES (?1, 'authenticated', ?2, ?3)
         ON CONFLICT(provider) DO UPDATE SET
           status = 'authenticated',
           email = excluded.email,
           updated_at = excluded.updated_at",
        libsql::params![provider, email, now],
    )
    .await
    .with_context(|| format!("Failed to save {provider} auth"))?;
    Ok(())
}

/// Check if a provider is authenticated. Returns `None` if no record exists.
pub async fn get_provider_auth(conn: &Connection, provider: &str) -> Result<Option<ProviderAuth>> {
    let mut rows = conn
        .query(
            "SELECT status, email FROM credentials WHERE provider = ?1",
            libsql::params![provider],
        )
        .await
        .with_context(|| format!("Failed to query {provider} auth"))?;

    let row = rows
        .next()
        .await
        .with_context(|| format!("Failed to read {provider} auth row"))?;
    match row {
        Some(row) => {
            let status: String = row.get(0).context("Failed to read status column")?;
            let email: Option<String> = row.get(1).ok();
            Ok(Some(ProviderAuth { status, email }))
        }
        None => Ok(None),
    }
}

/// Store (upsert) cursor authentication status.
pub async fn save_cursor_auth(conn: &Connection, email: &str) -> Result<()> {
    save_provider_auth(conn, "cursor", email).await
}

/// Check if cursor is authenticated. Returns `None` if no record exists.
pub async fn get_cursor_auth(conn: &Connection) -> Result<Option<ProviderAuth>> {
    get_provider_auth(conn, "cursor").await
}

/// Store (upsert) codex-auth OAuth token and optional account ID.
///
/// Uses the existing credentials table: token goes in `api_key`, account_id in `email`.
pub async fn save_codex_auth_token(
    conn: &Connection,
    token: &str,
    account_id: Option<&str>,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO credentials (provider, status, email, api_key, updated_at)
         VALUES ('codex-auth', 'authenticated', ?1, ?2, ?3)
         ON CONFLICT(provider) DO UPDATE SET
           status = 'authenticated',
           email = excluded.email,
           api_key = excluded.api_key,
           updated_at = excluded.updated_at",
        libsql::params![account_id.unwrap_or(""), token, now],
    )
    .await
    .context("Failed to save codex-auth token")?;
    Ok(())
}

/// Retrieve codex-auth OAuth token and optional account ID.
///
/// Returns `(token, Option<account_id>)` or `None` if not authenticated.
pub async fn get_codex_auth_token(conn: &Connection) -> Result<Option<(String, Option<String>)>> {
    let mut rows = conn
        .query(
            "SELECT api_key, email FROM credentials WHERE provider = 'codex-auth' AND status = 'authenticated'",
            (),
        )
        .await
        .context("Failed to query codex-auth token")?;

    let row = rows.next().await.context("Failed to read codex-auth row")?;
    match row {
        Some(row) => {
            let token: String = row.get(0).context("Failed to read api_key column")?;
            if token.is_empty() {
                return Ok(None);
            }
            let account_id: Option<String> = row
                .get::<String>(1)
                .ok()
                .and_then(|s| if s.is_empty() { None } else { Some(s) });
            Ok(Some((token, account_id)))
        }
        None => Ok(None),
    }
}
