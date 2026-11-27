//! Disk cleanup of old app images (after each deploy, on a schedule, or on demand) and
//! the release update check.

use std::sync::Arc;
use std::time::Duration;

use chrono::{SubsecRound, Timelike, Utc};
use serde::Serialize;

use crate::api::AppState;
use crate::backup::parse_cron;
use crate::db::{get_setting, set_setting};
use crate::error::{AppError, AppResult};
use crate::executor::NodeExecutor;

/// Settings key: 5-field UTC cron expression for automatic cleanup; absent = disabled.
pub const CLEANUP_CRON: &str = "cleanup_cron";
/// Settings key: images of this many recent healthy deployments are kept per app.
pub const CLEANUP_KEEP: &str = "cleanup_keep";
pub const DEFAULT_KEEP: i64 = 3;
pub const MAX_KEEP: i64 = 50;

#[derive(Debug, Clone, Serialize)]
pub struct CleanupConfig {
    pub cron_expr: Option<String>,
    pub keep_images: i64,
}

pub async fn config(state: &AppState) -> AppResult<CleanupConfig> {
    let keep_images = get_setting(&state.db, CLEANUP_KEEP)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_KEEP);
    Ok(CleanupConfig {
        cron_expr: get_setting(&state.db, CLEANUP_CRON).await?,
        keep_images,
    })
}

/// Validates both values before storing either. An empty cron disables the schedule.
pub async fn set_config(
    state: &AppState,
    cron_expr: Option<&str>,
    keep_images: i64,
) -> AppResult<CleanupConfig> {
    let cron_expr = cron_expr.map(str::trim).filter(|c| !c.is_empty());
    if let Some(expr) = cron_expr {
        parse_cron(expr)?;
    }
    if !(1..=MAX_KEEP).contains(&keep_images) {
        return Err(AppError::validation(format!(
            "keep_images must be between 1 and {MAX_KEEP}"
        )));
    }
    set_setting(&state.db, CLEANUP_CRON, cron_expr).await?;
    set_setting(&state.db, CLEANUP_KEEP, Some(&keep_images.to_string())).await?;
    config(state).await
}

/// Remove an app's old images, keeping the newest `keep_images` healthy deployments, the
/// active deployment and any queued or in-progress deployment's image. The caller must
/// hold the app's deploy lock so no deployment changes underneath.
pub async fn prune_app(
    state: &AppState,
    executor: &Arc<dyn NodeExecutor>,
    app_id: &str,
    app_name: &str,
) -> AppResult<Vec<String>> {
    let keep_images = config(state).await?.keep_images;
    let mut keep: Vec<String> = sqlx::query_scalar(
        "SELECT image_ref FROM deployments WHERE app_id = ? AND image_ref IS NOT NULL \
         AND status IN ('healthy', 'rolled_back') ORDER BY started_at DESC, id DESC LIMIT ?",
    )
    .bind(app_id)
    .bind(keep_images)
    .fetch_all(&state.db)
    .await?;
    let pinned: Vec<String> = sqlx::query_scalar(
        "SELECT image_ref FROM deployments WHERE app_id = ? AND image_ref IS NOT NULL \
         AND (status IN ('queued', 'building') \
              OR id = (SELECT active_deployment_id FROM apps WHERE id = ?))",
    )
    .bind(app_id)
    .bind(app_id)
    .fetch_all(&state.db)
    .await?;
    for image in pinned {
        if !keep.contains(&image) {
            keep.push(image);
        }
    }
    executor.prune_images(app_name, &keep).await
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CleanupReport {
    /// Old app image tags removed.
    pub removed_images: Vec<String>,
    /// Bytes freed by removing dangling images and unused build cache.
    pub reclaimed_bytes: u64,
    /// Apps or nodes that could not be cleaned, with the reason.
    pub errors: Vec<String>,
}

/// Prune every app's old images, then dangling images and build cache on every node.
/// Per-app or per-node failures are reported, not fatal.
pub async fn cleanup(state: &AppState) -> AppResult<CleanupReport> {
    let mut report = CleanupReport::default();
    let apps: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, name, node_id FROM apps ORDER BY name")
            .fetch_all(&state.db)
            .await?;
    for (app_id, name, node_id) in apps {
        let result = async {
            let executor = state.registry.get(&node_id)?;
            let lock = state.deploy_locks.get(&app_id);
            let _guard = lock.lock().await;
            prune_app(state, &executor, &app_id, &name).await
        }
        .await;
        match result {
            Ok(removed) => report.removed_images.extend(removed),
            Err(err) => report.errors.push(format!("{name}: {err}")),
        }
    }
    for node_id in state.registry.ids() {
        let result = match state.registry.get(&node_id) {
            Ok(executor) => executor.prune_unused().await,
            Err(err) => Err(err),
        };
        match result {
            Ok(bytes) => report.reclaimed_bytes += bytes,
            Err(err) => report.errors.push(format!("node {node_id}: {err}")),
        }
    }
    Ok(report)
}

/// Serializes cleanups (manual and scheduled) so two never run at once.
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run a cleanup, or `None` if one is already in progress.
pub async fn run_cleanup(state: &AppState) -> AppResult<Option<CleanupReport>> {
    let Ok(_guard) = RUNNING.try_lock() else {
        return Ok(None);
    };
    let report = cleanup(state).await?;
    tracing::info!(
        removed = report.removed_images.len(),
        reclaimed_bytes = report.reclaimed_bytes,
        errors = report.errors.len(),
        "disk cleanup finished"
    );
    Ok(Some(report))
}

/// Whether a stored cron expression matches `minute`.
fn due(expr: &Option<String>, minute: &chrono::DateTime<Utc>) -> bool {
    expr.as_deref()
        .and_then(|expr| parse_cron(expr).ok())
        .and_then(|cron| cron.is_time_matching(minute).ok())
        .unwrap_or(false)
}

/// Tick at the start of every minute and run the disk cleanup and the panel backup when
/// their schedules match.
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        loop {
            let wait = 60 - u64::from(Utc::now().second());
            tokio::time::sleep(Duration::from_secs(wait)).await;
            let minute = Utc::now()
                .trunc_subsecs(0)
                .with_second(0)
                .unwrap_or_else(Utc::now);
            match get_setting(&state.db, CLEANUP_CRON).await {
                Ok(expr) if due(&expr, &minute) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        match run_cleanup(&state).await {
                            Ok(Some(_)) => {}
                            Ok(None) => tracing::warn!("previous cleanup still running; skipped"),
                            Err(err) => tracing::warn!(error = %err, "scheduled cleanup failed"),
                        }
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "loading cleanup schedule failed"),
            }
            match crate::panel_backup::schedule(&state).await {
                Ok(expr) if due(&expr, &minute) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        match crate::panel_backup::run(&state).await {
                            Ok(Some(_)) => {}
                            Ok(None) => {
                                tracing::warn!("previous panel backup still running; skipped")
                            }
                            Err(err) => tracing::warn!(error = %err, "panel backup failed"),
                        }
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "loading panel backup schedule failed"),
            }
        }
    });
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Official releases; `FORGENERD_REPO` overrides it for forks.
pub const DEFAULT_REPO: &str = "forgenerd/forgenerd";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current: &'static str,
    /// Latest release tag, without a leading `v`.
    pub latest: String,
    pub update_available: bool,
    /// GitHub `owner/name` releases are checked against.
    pub repo: String,
    pub release_url: String,
}

/// Compare the running version with the repository's latest GitHub release.
pub async fn check_for_updates() -> AppResult<UpdateStatus> {
    let repo = std::env::var("FORGENERD_REPO")
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string());
    let valid = repo.split('/').count() == 2
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if !valid {
        return Err(AppError::validation(format!(
            "FORGENERD_REPO must be owner/name, got {repo:?}"
        )));
    }

    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        html_url: String,
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("forgenerd/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AppError::Internal(e.into()))?;
    let response = http
        .get(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("checking for updates: {e}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::Conflict(format!(
            "{repo} has no published releases"
        )));
    }
    let release: Release = response
        .error_for_status()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("checking for updates: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("reading release: {e}")))?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    Ok(UpdateStatus {
        current: VERSION,
        update_available: is_newer(&latest, VERSION),
        latest,
        repo,
        release_url: release.html_url,
    })
}

/// Numeric `major.minor.patch` comparison; a pre-release suffix is ignored.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    parts(candidate) > parts(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.10.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
    }
}
