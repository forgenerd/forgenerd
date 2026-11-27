use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::AppState;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::executor::DiskUsage;
use crate::maintenance::{self, CleanupConfig, CleanupReport, UpdateProgress, UpdateStatus};
use crate::panel_backup::{self, LastRun};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/updates", get(check_updates))
        .route("/updates/install", post(install_update))
        .route("/updates/progress", get(update_progress))
        .route("/cleanup", get(show_cleanup).patch(update_cleanup))
        .route("/cleanup/run", post(run_cleanup))
        .route("/disk", get(disk_usage))
        .route(
            "/panel-backup",
            get(show_panel_backup).patch(update_panel_backup),
        )
        .route("/panel-backup/run", post(run_panel_backup))
}

async fn check_updates(
    _: AuthUser,
    State(state): State<AppState>,
) -> AppResult<Json<UpdateStatus>> {
    let mut status = maintenance::check_for_updates().await?;
    status.self_update = maintenance::self_update_available(&state.data_dir);
    Ok(Json(status))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallUpdate {
    version: String,
}

/// Hands the request to the root-run updater; the panel restarts when it is done.
async fn install_update(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<InstallUpdate>,
) -> AppResult<(StatusCode, Json<UpdateProgress>)> {
    let progress = maintenance::request_update(&state.data_dir, &req.version).await?;
    Ok((StatusCode::ACCEPTED, Json(progress)))
}

async fn update_progress(
    _: AuthUser,
    State(state): State<AppState>,
) -> AppResult<Json<UpdateProgress>> {
    Ok(Json(maintenance::update_progress(&state.data_dir).await?))
}

async fn show_cleanup(
    _: AuthUser,
    State(state): State<AppState>,
) -> AppResult<Json<CleanupConfig>> {
    Ok(Json(maintenance::config(&state).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateCleanup {
    cron_expr: Option<String>,
    keep_images: i64,
}

async fn update_cleanup(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateCleanup>,
) -> AppResult<Json<CleanupConfig>> {
    Ok(Json(
        maintenance::set_config(&state, req.cron_expr.as_deref(), req.keep_images).await?,
    ))
}

async fn run_cleanup(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<CleanupReport>> {
    maintenance::run_cleanup(&state)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::Conflict("a cleanup is already running".into()))
}

/// Disk used by Docker on the local node, and free space where forgenerd keeps its data.
async fn disk_usage(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<DiskUsage>> {
    Ok(Json(state.registry.get("local")?.disk_usage().await?))
}

async fn show_panel_backup(
    _: AuthUser,
    State(state): State<AppState>,
) -> AppResult<Json<panel_backup::Config>> {
    Ok(Json(panel_backup::config(&state).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePanelBackup {
    s3_target_id: Option<String>,
    cron_expr: Option<String>,
    retention_count: i64,
}

async fn update_panel_backup(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdatePanelBackup>,
) -> AppResult<Json<panel_backup::Config>> {
    Ok(Json(
        panel_backup::set_config(
            &state,
            req.s3_target_id.as_deref(),
            req.cron_expr.as_deref(),
            req.retention_count,
        )
        .await?,
    ))
}

/// Back up now; responds with the outcome once the upload finishes.
async fn run_panel_backup(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<LastRun>> {
    panel_backup::run(&state)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::Conflict("a panel backup is already running".into()))
}
