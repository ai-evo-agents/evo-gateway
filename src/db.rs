use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database};
use tracing::info;

const DEFAULT_DB_PATH: &str = "gateway.db";

/// Credential record for the Cursor provider.
#[derive(Debug, Clone)]
pub struct CursorAuth {
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

/// Store (upsert) cursor authentication status.
pub async fn save_cursor_auth(conn: &Connection, email: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO credentials (provider, status, email, updated_at)
         VALUES ('cursor', 'authenticated', ?1, ?2)
         ON CONFLICT(provider) DO UPDATE SET
           status = 'authenticated',
           email = excluded.email,
           updated_at = excluded.updated_at",
        libsql::params![email, now],
    )
    .await
    .context("Failed to save cursor auth")?;
    Ok(())
}

/// Check if cursor is authenticated. Returns `None` if no record exists.
pub async fn get_cursor_auth(conn: &Connection) -> Result<Option<CursorAuth>> {
    let mut rows = conn
        .query(
            "SELECT status, email FROM credentials WHERE provider = 'cursor'",
            (),
        )
        .await
        .context("Failed to query cursor auth")?;

    let row = rows
        .next()
        .await
        .context("Failed to read cursor auth row")?;
    match row {
        Some(row) => {
            let status: String = row.get(0).context("Failed to read status column")?;
            let email: Option<String> = row.get(1).ok();
            Ok(Some(CursorAuth { status, email }))
        }
        None => Ok(None),
    }
}
