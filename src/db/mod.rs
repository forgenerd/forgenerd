pub mod models;

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Open (creating if needed) the SQLite database in WAL mode and run migrations.
pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let url = format!("sqlite://{}", path.display());
    let options = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    // Small pool keeps the idle footprint low; SQLite serializes writes anyway.
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .with_context(|| format!("opening database {}", path.display()))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    Ok(pool)
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub async fn get_setting(db: &SqlitePool, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await
}

/// Upsert a setting; `None` deletes it.
pub async fn set_setting(db: &SqlitePool, key: &str, value: Option<&str>) -> sqlx::Result<()> {
    match value {
        Some(value) => {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .execute(db)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM settings WHERE key = ?")
                .bind(key)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}

/// Deployments interrupted by a restart can never finish; mark them failed.
pub async fn fail_interrupted_deployments(db: &SqlitePool) -> sqlx::Result<u64> {
    let result = sqlx::query(
        "UPDATE deployments SET status = 'failed', finished_at = CURRENT_TIMESTAMP \
         WHERE status IN ('queued', 'building')",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE apps SET status = CASE WHEN active_deployment_id IS NULL THEN 'failed' ELSE 'running' END \
         WHERE status = 'building'",
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}
