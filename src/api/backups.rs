//! `/api/v1/s3-targets` and `/api/v1/backups`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::auth::AuthUser;
use crate::auth::crypto::aad;
use crate::backup::{self, Job, s3};
use crate::db::new_id;
use crate::error::{AppError, AppResult};
use crate::validate;

pub fn s3_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_targets).post(create_target))
        .route("/{id}", axum::routing::delete(delete_target))
        .route("/{id}/test", post(test_target))
}

pub fn backup_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_jobs).post(create_job))
        .route("/{id}", get(show_job).patch(update_job).delete(delete_job))
        .route("/{id}/run", post(run_job))
        .route("/{id}/runs", get(list_runs))
        .route("/{id}/runs/{run_id}/restore", post(restore_run))
}

// ---------- S3 targets ----------

#[derive(Serialize, sqlx::FromRow)]
struct Target {
    id: String,
    name: String,
    endpoint: String,
    bucket: String,
    region: String,
    last_test_at: Option<NaiveDateTime>,
    last_test_ok: Option<bool>,
    last_test_error: Option<String>,
}

async fn list_targets(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<Vec<Target>>> {
    Ok(Json(
        sqlx::query_as::<_, Target>(
            "SELECT id, name, endpoint, bucket, region, last_test_at, last_test_ok, last_test_error \
             FROM s3_targets ORDER BY name",
        )
        .fetch_all(&state.db)
        .await?,
    ))
}

#[derive(Deserialize)]
struct CreateTarget {
    name: String,
    endpoint: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
}

/// S3 bucket naming rules (3-63 chars, lowercase letters, digits, dots, hyphens).
fn validate_bucket(bucket: &str) -> AppResult<()> {
    let ok = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && bucket.as_bytes()[0].is_ascii_alphanumeric()
        && bucket.as_bytes()[bucket.len() - 1].is_ascii_alphanumeric();
    if ok {
        Ok(())
    } else {
        Err(AppError::validation("invalid bucket name"))
    }
}

async fn create_target(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTarget>,
) -> AppResult<(StatusCode, Json<Target>)> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::validation("name must be 1-100 characters"));
    }
    let (endpoint, _) =
        s3::parse_endpoint(&req.endpoint).map_err(|e| AppError::validation(e.to_string()))?;
    validate_bucket(&req.bucket)?;
    let region = req.region.trim();
    if region.is_empty()
        || region.len() > 64
        || !region
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(AppError::validation(
            "invalid region (use e.g. us-east-1, or auto for R2)",
        ));
    }
    for (field, value) in [
        ("access_key", &req.access_key),
        ("secret_key", &req.secret_key),
    ] {
        if value.trim().is_empty() || value.len() > 512 {
            return Err(AppError::validation(format!(
                "{field} must be 1-512 characters"
            )));
        }
    }
    let id = new_id();
    let access = state.cipher.encrypt(
        req.access_key.trim(),
        &aad("s3_targets", &id, "access_key_enc"),
    )?;
    let secret = state.cipher.encrypt(
        req.secret_key.trim(),
        &aad("s3_targets", &id, "secret_key_enc"),
    )?;
    sqlx::query(
        "INSERT INTO s3_targets (id, name, endpoint, bucket, region, access_key_enc, secret_key_enc) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(&endpoint)
    .bind(&req.bucket)
    .bind(region)
    .bind(access)
    .bind(secret)
    .execute(&state.db)
    .await?;
    let target = sqlx::query_as::<_, Target>(
        "SELECT id, name, endpoint, bucket, region, last_test_at, last_test_ok, last_test_error \
         FROM s3_targets WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(target)))
}

async fn delete_target(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backup_jobs WHERE s3_target_id = ?")
        .bind(&id)
        .fetch_one(&state.db)
        .await?;
    if jobs > 0 {
        return Err(AppError::Conflict(
            "delete the backup jobs using this target first".into(),
        ));
    }
    let result = sqlx::query("DELETE FROM s3_targets WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Check credentials and bucket access with a cheap listing.
async fn test_target(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let (endpoint, bucket, region, access_enc, secret_enc): (String, String, String, String, String) =
        sqlx::query_as("SELECT endpoint, bucket, region, access_key_enc, secret_key_enc FROM s3_targets WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let client = s3::S3Client::new(
        &endpoint,
        &bucket,
        &region,
        &state
            .cipher
            .decrypt(&access_enc, &aad("s3_targets", &id, "access_key_enc"))?,
        &state
            .cipher
            .decrypt(&secret_enc, &aad("s3_targets", &id, "secret_key_enc"))?,
    )?;
    let result = client.list("forgenerd-connection-test/").await;
    // Record the outcome so the UI can show when the target last worked.
    sqlx::query(
        "UPDATE s3_targets SET last_test_at = CURRENT_TIMESTAMP, last_test_ok = ?, last_test_error = ? \
         WHERE id = ?",
    )
    .bind(result.is_ok())
    .bind(result.as_ref().err().map(|e| format!("{e:#}")))
    .bind(&id)
    .execute(&state.db)
    .await?;
    result.map_err(|e| AppError::validation(format!("S3 check failed: {e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- Backup jobs ----------

#[derive(Serialize)]
struct JobView {
    id: String,
    app_id: String,
    s3_target_id: String,
    driver: String,
    compose_service: Option<String>,
    volume_id: Option<String>,
    db_name: Option<String>,
    db_user: Option<String>,
    has_db_password: bool,
    cron_expr: String,
    retention_count: i64,
    enabled: bool,
    /// Next scheduled run (UTC), when enabled.
    next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    last_run: Option<Run>,
}

#[derive(Serialize, sqlx::FromRow)]
struct Run {
    id: String,
    status: String,
    trigger: String,
    object_key: Option<String>,
    size_bytes: Option<i64>,
    error: Option<String>,
    started_at: NaiveDateTime,
    finished_at: Option<NaiveDateTime>,
}

async fn view(state: &AppState, job: Job) -> AppResult<JobView> {
    let last_run = sqlx::query_as::<_, Run>(
        "SELECT id, status, trigger, object_key, size_bytes, error, started_at, finished_at \
         FROM backup_runs WHERE job_id = ? ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .bind(&job.id)
    .fetch_optional(&state.db)
    .await?;
    let next_run_at = if job.enabled {
        backup::parse_cron(&job.cron_expr)
            .ok()
            .and_then(|cron| cron.find_next_occurrence(&chrono::Utc::now(), false).ok())
    } else {
        None
    };
    Ok(JobView {
        next_run_at,
        has_db_password: job.db_password_enc.is_some(),
        id: job.id,
        app_id: job.app_id,
        s3_target_id: job.s3_target_id,
        driver: job.driver,
        compose_service: job.compose_service,
        volume_id: job.volume_id,
        db_name: job.db_name,
        db_user: job.db_user,
        cron_expr: job.cron_expr,
        retention_count: job.retention_count,
        enabled: job.enabled,
        last_run,
    })
}

async fn fetch_job(state: &AppState, id: &str) -> AppResult<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM backup_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn list_jobs(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<Vec<JobView>>> {
    let jobs = sqlx::query_as::<_, Job>("SELECT * FROM backup_jobs ORDER BY id")
        .fetch_all(&state.db)
        .await?;
    let mut out = Vec::with_capacity(jobs.len());
    for job in jobs {
        out.push(view(&state, job).await?);
    }
    Ok(Json(out))
}

async fn show_job(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<JobView>> {
    let job = fetch_job(&state, &id).await?;
    Ok(Json(view(&state, job).await?))
}

#[derive(Deserialize)]
struct CreateJob {
    app_id: String,
    s3_target_id: String,
    driver: String,
    cron_expr: String,
    #[serde(default = "default_retention")]
    retention_count: i64,
    compose_service: Option<String>,
    /// Name of one of the app's volumes (driver `volume`).
    volume: Option<String>,
    db_name: Option<String>,
    db_user: Option<String>,
    db_password: Option<String>,
}

fn default_retention() -> i64 {
    7
}

fn validate_retention(n: i64) -> AppResult<()> {
    if (1..=365).contains(&n) {
        Ok(())
    } else {
        Err(AppError::validation("retention_count must be 1-365"))
    }
}

fn validate_db_field(field: &str, value: &Option<String>) -> AppResult<()> {
    match value {
        Some(v) if v.is_empty() || v.len() > 256 || v.contains('\0') => Err(AppError::validation(
            format!("{field} must be 1-256 characters"),
        )),
        _ => Ok(()),
    }
}

async fn create_job(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateJob>,
) -> AppResult<(StatusCode, Json<JobView>)> {
    backup::parse_cron(&req.cron_expr)?;
    validate_retention(req.retention_count)?;
    for (field, value) in [
        ("db_name", &req.db_name),
        ("db_user", &req.db_user),
        ("db_password", &req.db_password),
    ] {
        validate_db_field(field, value)?;
    }
    let app = sqlx::query_as::<_, crate::db::models::App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(&req.app_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::validation("app_id does not exist"))?;
    let target: Option<String> = sqlx::query_scalar("SELECT id FROM s3_targets WHERE id = ?")
        .bind(&req.s3_target_id)
        .fetch_optional(&state.db)
        .await?;
    if target.is_none() {
        return Err(AppError::validation("s3_target_id does not exist"));
    }

    let mut volume_id = None;
    match req.driver.as_str() {
        "volume" => {
            if app.deploy_type == "compose" {
                return Err(AppError::validation(
                    "volume backups apply to image/git app volumes; use postgres/mysql for compose databases",
                ));
            }
            let name = req
                .volume
                .as_deref()
                .ok_or_else(|| AppError::validation("volume is required"))?;
            volume_id = Some(
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM volumes WHERE app_id = ? AND name = ?",
                )
                .bind(&app.id)
                .bind(name)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::validation(format!("app has no volume named {name}")))?,
            );
        }
        "postgres" | "mysql" => {
            if let Some(service) = &req.compose_service {
                if app.deploy_type != "compose" {
                    return Err(AppError::validation(
                        "compose_service only applies to compose apps",
                    ));
                }
                let yaml: Option<String> =
                    sqlx::query_scalar("SELECT compose_yaml FROM apps WHERE id = ?")
                        .bind(&app.id)
                        .fetch_one(&state.db)
                        .await?;
                let services = validate::compose::check_raw(&yaml.unwrap_or_default())?;
                if !services.contains(service) {
                    return Err(AppError::validation(format!(
                        "compose_service `{service}` is not in the app's compose file"
                    )));
                }
            } else if app.deploy_type == "compose" && app.compose_service.is_none() {
                return Err(AppError::validation(
                    "compose_service is required for this app",
                ));
            }
        }
        _ => {
            return Err(AppError::validation(
                "driver must be postgres, mysql, or volume",
            ));
        }
    }

    let id = new_id();
    let password_enc = req
        .db_password
        .as_deref()
        .map(|p| {
            state
                .cipher
                .encrypt(p, &aad("backup_jobs", &id, "db_password_enc"))
        })
        .transpose()?;
    sqlx::query(
        "INSERT INTO backup_jobs (id, app_id, s3_target_id, driver, compose_service, volume_id, db_name, \
         db_user, db_password_enc, cron_expr, retention_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&app.id)
    .bind(&req.s3_target_id)
    .bind(&req.driver)
    .bind(&req.compose_service)
    .bind(volume_id)
    .bind(&req.db_name)
    .bind(&req.db_user)
    .bind(password_enc)
    .bind(req.cron_expr.trim())
    .bind(req.retention_count)
    .execute(&state.db)
    .await?;
    let job = fetch_job(&state, &id).await?;
    Ok((StatusCode::CREATED, Json(view(&state, job).await?)))
}

#[derive(Deserialize)]
struct UpdateJob {
    cron_expr: Option<String>,
    retention_count: Option<i64>,
    enabled: Option<bool>,
}

async fn update_job(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateJob>,
) -> AppResult<Json<JobView>> {
    fetch_job(&state, &id).await?;
    if let Some(expr) = &req.cron_expr {
        backup::parse_cron(expr)?;
    }
    if let Some(n) = req.retention_count {
        validate_retention(n)?;
    }
    sqlx::query(
        "UPDATE backup_jobs SET cron_expr = COALESCE(?, cron_expr), \
         retention_count = COALESCE(?, retention_count), enabled = COALESCE(?, enabled) WHERE id = ?",
    )
    .bind(req.cron_expr.as_deref().map(str::trim))
    .bind(req.retention_count)
    .bind(req.enabled)
    .bind(&id)
    .execute(&state.db)
    .await?;
    let job = fetch_job(&state, &id).await?;
    Ok(Json(view(&state, job).await?))
}

async fn delete_job(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query("DELETE FROM backup_jobs WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct RunStarted {
    run_id: String,
}

async fn run_job(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<RunStarted>)> {
    fetch_job(&state, &id).await?;
    let run_id = backup::start_detached(&state, &id).await?;
    Ok((StatusCode::ACCEPTED, Json(RunStarted { run_id })))
}

async fn list_runs(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<Run>>> {
    fetch_job(&state, &id).await?;
    Ok(Json(
        sqlx::query_as::<_, Run>(
            "SELECT id, status, trigger, object_key, size_bytes, error, started_at, finished_at \
             FROM backup_runs WHERE job_id = ? ORDER BY started_at DESC, id DESC LIMIT 50",
        )
        .bind(&id)
        .fetch_all(&state.db)
        .await?,
    ))
}

/// Replace the app's current data with this run's backup. Runs to completion before
/// responding.
async fn restore_run(
    _: AuthUser,
    State(state): State<AppState>,
    Path((id, run_id)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    backup::restore(&state, &id, &run_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
