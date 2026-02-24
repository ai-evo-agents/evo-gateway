use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database};
use tracing::info;

const DEFAULT_DB_PATH: &str = "gateway.db";

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
