use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::AppState;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::executor::DiskUsage;
use crate::maintenance::{self, CleanupConfig, CleanupReport, UpdateStatus};
use crate::panel_backup::{self, LastRun};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/updates", get(check_updates))
        .route("/cleanup", get(show_cleanup).patch(update_cleanup))
        .route("/cleanup/run", post(run_cleanup))
        .route("/disk", get(disk_usage))
        .route(
            "/panel-backup",
            get(show_panel_backup).patch(update_panel_backup),
        )
        .route("/panel-backup/run", post(run_panel_backup))
}

async fn check_updates(_: AuthUser) -> AppResult<Json<UpdateStatus>> {
    Ok(Json(maintenance::check_for_updates().await?))
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
