//! Disk cleanup of old app images (after each deploy, on a schedule, or on demand), the
//! release update check, and updates requested from the panel.

use std::path::Path;
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
    /// The installer set up the updater, so the panel can install `latest` itself.
    pub self_update: bool,
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
        self_update: false,
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

// ---------------------------------------------------------------- panel updates
//
// The panel never installs anything itself: it runs unprivileged and cannot replace its
// own binary. The installer creates `<data_dir>/update/` and a root-owned helper that a
// systemd path unit runs whenever `update/request` appears. The panel only writes the
// release tag into that file; the helper deletes the request, runs that release's
// installer (which verifies the binary's checksum and restarts the panel), appends its
// output to `update/log` and writes `ok <tag>` or `failed <tag>` to `update/result`.

const UPDATE_DIR: &str = "update";
/// Shown with a failed update; the installer's output before the failure is enough.
const LOG_TAIL_LINES: usize = 60;
const LOG_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// No update requested (or the last one's result was never recorded).
    Idle,
    /// Requested; the updater has not picked it up yet.
    Pending,
    /// The updater is running the installer.
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgress {
    pub state: UpdateState,
    /// Release tag being (or last) installed, e.g. `v0.2.0`.
    pub version: Option<String>,
    /// Version of the binary serving this response.
    pub current: &'static str,
    /// End of the updater's output; empty unless the update failed.
    pub log: String,
}

/// The installer created the updater (older installs need one more manual run).
pub fn self_update_available(data_dir: &Path) -> bool {
    data_dir.join(UPDATE_DIR).is_dir()
}

/// Ask the updater to install release `version` (`0.2.0` or `v0.2.0`).
pub async fn request_update(data_dir: &Path, version: &str) -> AppResult<UpdateProgress> {
    let number = version.trim().trim_start_matches('v');
    let valid = !number.is_empty()
        && number.starts_with(|c: char| c.is_ascii_digit())
        && number.len() <= 32
        && number
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    if !valid {
        return Err(AppError::validation(format!(
            "invalid release version: {version:?}"
        )));
    }
    if !is_newer(number, VERSION) {
        return Err(AppError::validation(format!(
            "{number} is not newer than the running version {VERSION}"
        )));
    }
    if !self_update_available(data_dir) {
        return Err(AppError::Conflict(
            "updating from the panel needs the updater: re-run the installer on the server once"
                .into(),
        ));
    }
    let progress = update_progress(data_dir).await?;
    if matches!(progress.state, UpdateState::Pending | UpdateState::Running) {
        return Err(AppError::Conflict(
            "an update is already in progress".into(),
        ));
    }

    let dir = data_dir.join(UPDATE_DIR);
    let tag = format!("v{number}");
    remove_if_present(&dir.join("result")).await?;
    let header = format!("Update to {tag} requested at {}\n", Utc::now().to_rfc3339());
    tokio::fs::write(dir.join("log"), header)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    // Renamed into place, so the path unit never sees a half-written request.
    let pending = dir.join("request.tmp");
    tokio::fs::write(&pending, format!("{tag}\n"))
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    tokio::fs::rename(&pending, dir.join("request"))
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    update_progress(data_dir).await
}

pub async fn update_progress(data_dir: &Path) -> AppResult<UpdateProgress> {
    let dir = data_dir.join(UPDATE_DIR);
    let request = read_optional(&dir.join("request")).await?;
    let result = read_optional(&dir.join("result")).await?;
    let log_exists = tokio::fs::try_exists(dir.join("log"))
        .await
        .unwrap_or(false);

    let (state, version) = if let Some(request) = request {
        (UpdateState::Pending, Some(request.trim().to_string()))
    } else if let Some(result) = result {
        let mut parts = result.split_whitespace();
        let outcome = parts.next().unwrap_or("");
        let version = parts.next().map(str::to_string);
        let state = if outcome == "ok" {
            UpdateState::Succeeded
        } else {
            UpdateState::Failed
        };
        (state, version)
    } else if log_exists {
        (UpdateState::Running, None)
    } else {
        (UpdateState::Idle, None)
    };
    let log = if state == UpdateState::Failed {
        log_tail(&dir.join("log")).await
    } else {
        String::new()
    };
    Ok(UpdateProgress {
        state,
        version,
        current: VERSION,
        log,
    })
}

async fn read_optional(path: &Path) -> AppResult<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::Internal(e.into())),
    }
}

async fn remove_if_present(path: &Path) -> AppResult<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AppError::Internal(e.into())),
    }
}

async fn log_tail(path: &Path) -> String {
    let Ok(bytes) = tokio::fs::read(path).await else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    let text = strip_ansi(&String::from_utf8_lossy(&bytes[start..]));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
}

/// Drop terminal escape sequences (`ESC [ ... letter`) that installers may print.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{is_newer, strip_ansi};

    #[test]
    fn strips_terminal_colors() {
        assert_eq!(
            strip_ansi("\u{1b}[31mx run as root\u{1b}[0m done"),
            "x run as root done"
        );
        assert_eq!(strip_ansi("plain [text]"), "plain [text]");
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.10.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
    }
}
