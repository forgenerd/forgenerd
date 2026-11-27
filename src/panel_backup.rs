//! Backups of forgenerd's own database to an S3 target, so a lost server can be rebuilt
//! with every app, domain, secret and job intact. The master key that encrypts secrets
//! is never uploaded: it must be kept separately, or the backup alone reveals nothing.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use chrono::Utc;
use serde::Serialize;

use crate::api::AppState;
use crate::backup::{parse_cron, s3::S3Client, s3_destination};
use crate::db::{get_setting, set_setting};
use crate::error::{AppError, AppResult};

const TARGET: &str = "panel_backup_target";
const CRON: &str = "panel_backup_cron";
const RETENTION: &str = "panel_backup_retention";
const LAST: &str = "panel_backup_last";
pub const DEFAULT_RETENTION: i64 = 14;
pub const MAX_RETENTION: i64 = 365;
/// Objects live under this prefix in the target bucket.
pub const PREFIX: &str = "forgenerd/panel/";

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub s3_target_id: Option<String>,
    /// UTC schedule; `None` = manual only.
    pub cron_expr: Option<String>,
    pub retention_count: i64,
    pub last_run: Option<LastRun>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LastRun {
    pub finished_at: chrono::DateTime<Utc>,
    pub ok: bool,
    pub object_key: Option<String>,
    pub size_bytes: Option<u64>,
    pub error: Option<String>,
}

pub async fn config(state: &AppState) -> AppResult<Config> {
    Ok(Config {
        s3_target_id: get_setting(&state.db, TARGET).await?,
        cron_expr: get_setting(&state.db, CRON).await?,
        retention_count: get_setting(&state.db, RETENTION)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RETENTION),
        last_run: get_setting(&state.db, LAST)
            .await?
            .and_then(|v| serde_json::from_str(&v).ok()),
    })
}

/// Validates every value before storing any. A schedule needs a target.
pub async fn set_config(
    state: &AppState,
    s3_target_id: Option<&str>,
    cron_expr: Option<&str>,
    retention_count: i64,
) -> AppResult<Config> {
    let target = s3_target_id.map(str::trim).filter(|t| !t.is_empty());
    let cron = cron_expr.map(str::trim).filter(|c| !c.is_empty());
    if let Some(target) = target {
        let exists: Option<String> = sqlx::query_scalar("SELECT id FROM s3_targets WHERE id = ?")
            .bind(target)
            .fetch_optional(&state.db)
            .await?;
        if exists.is_none() {
            return Err(AppError::validation("S3 target not found"));
        }
    }
    if let Some(cron) = cron {
        if target.is_none() {
            return Err(AppError::validation(
                "choose an S3 target to schedule backups",
            ));
        }
        parse_cron(cron)?;
    }
    if !(1..=MAX_RETENTION).contains(&retention_count) {
        return Err(AppError::validation(format!(
            "retention_count must be between 1 and {MAX_RETENTION}"
        )));
    }
    set_setting(&state.db, TARGET, target).await?;
    set_setting(&state.db, CRON, cron).await?;
    set_setting(&state.db, RETENTION, Some(&retention_count.to_string())).await?;
    config(state).await
}

pub async fn schedule(state: &AppState) -> AppResult<Option<String>> {
    Ok(get_setting(&state.db, CRON).await?)
}

/// Serializes panel backups so two never run at once.
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Back up now and record the outcome; `None` if a backup is already running.
pub async fn run(state: &AppState) -> AppResult<Option<LastRun>> {
    let Ok(_guard) = RUNNING.try_lock() else {
        return Ok(None);
    };
    let result = upload(state).await;
    let last = match &result {
        Ok((key, size)) => LastRun {
            finished_at: Utc::now(),
            ok: true,
            object_key: Some(key.clone()),
            size_bytes: Some(*size),
            error: None,
        },
        Err(err) => LastRun {
            finished_at: Utc::now(),
            ok: false,
            object_key: None,
            size_bytes: None,
            error: Some(err.to_string()),
        },
    };
    let json = serde_json::to_string(&last).map_err(|e| anyhow!(e))?;
    set_setting(&state.db, LAST, Some(&json)).await?;
    match &result {
        Ok((key, size)) => tracing::info!(key = %key, bytes = size, "panel backup succeeded"),
        Err(err) => {
            tracing::warn!(error = %err, "panel backup failed");
            crate::notify::dispatch(
                state.db.clone(),
                state.cipher.clone(),
                crate::notify::Event {
                    kind: crate::notify::BACKUP_FAILED,
                    app_id: String::new(),
                    app_name: "forgenerd panel".into(),
                    from: "database".into(),
                    to: "failed".into(),
                    detail: err.to_string(),
                },
            );
        }
    }
    Ok(Some(last))
}

async fn upload(state: &AppState) -> AppResult<(String, u64)> {
    let target = get_setting(&state.db, TARGET)
        .await?
        .ok_or_else(|| AppError::validation("choose an S3 target first"))?;
    let retention = config(state).await?.retention_count;
    let dest = s3_destination(state, &target, PREFIX.into(), 1).await?;
    let client = S3Client::new(
        &dest.endpoint,
        &dest.bucket,
        &dest.region,
        &dest.access_key,
        &dest.secret_key,
    )?;
    let dir = temp_dir(&state.data_dir)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let snapshot = dir.join(format!("{stamp}.db"));
    let archive = dir.join(format!("{stamp}.db.gz"));
    let result = async {
        // VACUUM INTO writes a consistent, compacted copy of the live database.
        let path = snapshot.to_str().context("non-UTF-8 data directory")?;
        sqlx::query("VACUUM INTO ?")
            .bind(path)
            .execute(&state.db)
            .await
            .context("snapshotting the database")?;
        gzip(&snapshot, &archive).await?;
        let key = format!("{PREFIX}{stamp}-forgenerd.db.gz");
        let size = client.put_file(&key, &archive).await?;
        let mut objects = client.list(PREFIX).await?;
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        let excess = objects
            .len()
            .saturating_sub(usize::try_from(retention).unwrap_or(1));
        for object in objects.into_iter().take(excess) {
            client.delete(&object.key).await?;
        }
        Ok::<_, anyhow::Error>((key, size))
    }
    .await;
    let _ = tokio::fs::remove_file(&snapshot).await;
    let _ = tokio::fs::remove_file(&archive).await;
    result.map_err(AppError::Internal)
}

fn temp_dir(data_dir: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let dir = data_dir.join("backups").join("tmp");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

async fn gzip(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&dst)?;
        let mut encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        std::io::copy(&mut std::fs::File::open(&src)?, &mut encoder)?;
        encoder.finish()?.flush()?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| anyhow!(e))?
}
