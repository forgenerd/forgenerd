//! Backup jobs: a once-a-minute scheduler that reads enabled jobs from SQLite (so
//! create/edit/delete need no scheduler bookkeeping), a runner that resolves the job into
//! a `BackupSpec` for the app's node executor, and restores of a finished run.

pub mod s3;

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use croner::Cron;

use crate::api::AppState;
use crate::auth::crypto::aad;
use crate::db::models::App;
use crate::db::new_id;
use crate::error::{AppError, AppResult};
use crate::executor::{
    BackupSource, BackupSpec, ContainerAction, RestoreSpec, S3Destination, compose_container_name,
    volume_name,
};
use crate::notify::{self, Event};

/// Parse a standard 5-field cron expression (evaluated in UTC).
pub fn parse_cron(expr: &str) -> AppResult<Cron> {
    let fields = expr.split_whitespace().count();
    if fields != 5 {
        return Err(AppError::validation(
            "cron_expr must have 5 fields: minute hour day-of-month month day-of-week (UTC)",
        ));
    }
    Cron::from_str(expr).map_err(|e| AppError::validation(format!("invalid cron_expr: {e}")))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: String,
    pub app_id: String,
    pub s3_target_id: String,
    pub driver: String,
    pub compose_service: Option<String>,
    pub volume_id: Option<String>,
    pub db_name: Option<String>,
    pub db_user: Option<String>,
    pub db_password_enc: Option<String>,
    pub cron_expr: String,
    pub retention_count: i64,
    pub enabled: bool,
}

/// Jobs whose schedule matches `minute` (seconds ignored).
pub async fn due_jobs(state: &AppState, minute: DateTime<Utc>) -> AppResult<Vec<String>> {
    let minute = minute
        .with_second(0)
        .and_then(|m| m.with_nanosecond(0))
        .unwrap_or(minute);
    let jobs: Vec<(String, String)> =
        sqlx::query_as("SELECT id, cron_expr FROM backup_jobs WHERE enabled = 1")
            .fetch_all(&state.db)
            .await?;
    Ok(jobs
        .into_iter()
        .filter(|(id, expr)| match parse_cron(expr) {
            Ok(cron) => cron.is_time_matching(&minute).unwrap_or(false),
            Err(_) => {
                tracing::warn!(job = %id, "skipping backup job with invalid cron");
                false
            }
        })
        .map(|(id, _)| id)
        .collect())
}

/// Tick at the start of every minute and start due jobs in the background.
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        loop {
            let now = Utc::now();
            let wait = 60 - u64::from(now.second());
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match due_jobs(&state, Utc::now()).await {
                Ok(ids) => {
                    for id in ids {
                        let state = state.clone();
                        tokio::spawn(async move {
                            match start(&state, &id, "schedule").await {
                                Ok(Some((_, run))) => run.await,
                                Ok(None) => {
                                    tracing::warn!(job = %id, "previous run still in progress; skipped")
                                }
                                Err(err) => {
                                    tracing::warn!(job = %id, error = %err, "backup could not start")
                                }
                            }
                        });
                    }
                }
                Err(err) => tracing::warn!(error = %err, "loading backup jobs failed"),
            }
        }
    });
}

/// Start a run: records it and returns its id and future, or `None` if the job is
/// already running. Resolution errors (missing app/container) become a failed run.
pub async fn start(
    state: &AppState,
    job_id: &str,
    trigger: &'static str,
) -> AppResult<Option<(String, impl Future<Output = ()> + use<>)>> {
    let lock = state.deploy_locks.get(&format!("backup:{job_id}"));
    let Ok(guard) = lock.clone().try_lock_owned() else {
        return Ok(None);
    };
    let job = sqlx::query_as::<_, Job>("SELECT * FROM backup_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let run_id = new_id();
    sqlx::query(
        "INSERT INTO backup_runs (id, job_id, status, trigger) VALUES (?, ?, 'running', ?)",
    )
    .bind(&run_id)
    .bind(&job.id)
    .bind(trigger)
    .execute(&state.db)
    .await?;
    let state = state.clone();
    let id = run_id.clone();
    Ok(Some((id, async move {
        let _guard = guard;
        run(&state, &job, &run_id).await;
    })))
}

/// Spawn a run in the background; returns its id (for the manual-run endpoint).
pub async fn start_detached(state: &AppState, job_id: &str) -> AppResult<String> {
    let (run_id, run) = start(state, job_id, "manual")
        .await?
        .ok_or_else(|| AppError::Conflict("this backup is already running".into()))?;
    tokio::spawn(run);
    Ok(run_id)
}

async fn run(state: &AppState, job: &Job, run_id: &str) {
    let app = sqlx::query_as::<_, App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(&job.app_id)
    .fetch_optional(&state.db)
    .await;
    let result = match app {
        Ok(Some(app)) => execute(state, job, &app).await.map(|r| (app, r)),
        Ok(None) => Err(AppError::NotFound),
        Err(err) => Err(err.into()),
    };
    let update = match &result {
        Ok((_, r)) => sqlx::query(
            "UPDATE backup_runs SET status = 'succeeded', object_key = ?, size_bytes = ?, \
             finished_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(&r.object_key)
        .bind(i64::try_from(r.size_bytes).unwrap_or(i64::MAX))
        .bind(run_id),
        Err(err) => sqlx::query(
            "UPDATE backup_runs SET status = 'failed', error = ?, finished_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(err.to_string())
        .bind(run_id),
    };
    if let Err(err) = update.execute(&state.db).await {
        tracing::error!(error = %err, "recording backup run");
    }
    match result {
        Ok((app, r)) => {
            tracing::info!(app = %app.name, key = %r.object_key, bytes = r.size_bytes, pruned = r.pruned.len(), "backup succeeded")
        }
        Err(err) => {
            tracing::warn!(job = %job.id, error = %err, "backup failed");
            let app_name: Option<String> = sqlx::query_scalar("SELECT name FROM apps WHERE id = ?")
                .bind(&job.app_id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten();
            notify::dispatch(
                state.db.clone(),
                state.cipher.clone(),
                Event {
                    kind: notify::BACKUP_FAILED,
                    app_id: job.app_id.clone(),
                    app_name: app_name.unwrap_or_else(|| job.app_id.clone()),
                    from: job.driver.clone(),
                    to: "failed".into(),
                    detail: err.to_string(),
                },
            );
        }
    }
}

async fn execute(
    state: &AppState,
    job: &Job,
    app: &App,
) -> AppResult<crate::executor::BackupResult> {
    let source = resolve_source(state, job, app).await?;
    let prefix = format!("backups/{}/{}/", app.name, job.id);
    let object_key = format!(
        "{prefix}{}-{}.{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        source.driver(),
        source.extension()
    );
    let destination = s3_destination(
        state,
        &job.s3_target_id,
        prefix,
        u32::try_from(job.retention_count).unwrap_or(1),
    )
    .await?;
    let spec = BackupSpec {
        source,
        destination,
        object_key,
    };
    state.registry.get(&app.node_id)?.run_backup(&spec).await
}

/// Decrypted connection details of an S3 target.
pub async fn s3_destination(
    state: &AppState,
    target_id: &str,
    prefix: String,
    retention_count: u32,
) -> AppResult<S3Destination> {
    let (endpoint, bucket, region, access_enc, secret_enc): (String, String, String, String, String) =
        sqlx::query_as(
            "SELECT endpoint, bucket, region, access_key_enc, secret_key_enc FROM s3_targets WHERE id = ?",
        )
        .bind(target_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::validation("S3 target no longer exists"))?;
    Ok(S3Destination {
        endpoint,
        bucket,
        region,
        access_key: state
            .cipher
            .decrypt(&access_enc, &aad("s3_targets", target_id, "access_key_enc"))?,
        secret_key: state
            .cipher
            .decrypt(&secret_enc, &aad("s3_targets", target_id, "secret_key_enc"))?,
        prefix,
        retention_count,
    })
}

/// Restore a succeeded run of a job into the job's app, replacing the current data.
/// Database restores run against the live database. Volume restores stop the app's
/// container for the duration and start it again afterwards.
pub async fn restore(state: &AppState, job_id: &str, run_id: &str) -> AppResult<()> {
    let job = sqlx::query_as::<_, Job>("SELECT * FROM backup_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let (status, object_key): (String, Option<String>) =
        sqlx::query_as("SELECT status, object_key FROM backup_runs WHERE id = ? AND job_id = ?")
            .bind(run_id)
            .bind(job_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let object_key = object_key
        .filter(|_| status == "succeeded")
        .ok_or_else(|| AppError::Conflict("only a succeeded backup can be restored".into()))?;

    // No backup of this job, no deployment and no start/stop of the app may run meanwhile.
    let backup_lock = state.deploy_locks.get(&format!("backup:{job_id}"));
    let Ok(_backup_guard) = backup_lock.try_lock_owned() else {
        return Err(AppError::Conflict(
            "this backup is running; try again when it finishes".into(),
        ));
    };
    let deploy_lock = state.deploy_locks.get(&job.app_id);
    let Ok(_deploy_guard) = deploy_lock.try_lock_owned() else {
        return Err(AppError::Conflict(
            "a deployment is in progress; try again when it finishes".into(),
        ));
    };
    let container_lock = state.container_locks.get(&job.app_id);
    let _container_guard = container_lock.lock().await;

    let app = sqlx::query_as::<_, App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(&job.app_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let spec = RestoreSpec {
        target: resolve_source(state, &job, &app).await?,
        source: s3_destination(state, &job.s3_target_id, String::new(), 1).await?,
        object_key,
    };
    let executor = state.registry.get(&app.node_id)?;
    // Volume restores replace files under a running app; stop it first.
    let stop = (job.driver == "volume" && app.status == "running")
        .then(|| app.active_container_id.clone())
        .flatten();
    if let Some(container) = &stop {
        executor
            .set_container_state(container, ContainerAction::Stop)
            .await?;
    }
    let result = executor.run_restore(&spec).await;
    if let Some(container) = &stop
        && let Err(err) = executor
            .set_container_state(container, ContainerAction::Start)
            .await
    {
        tracing::error!(app = %app.name, error = %err, "could not start the app after a restore");
        if result.is_ok() {
            return Err(err);
        }
    }
    match &result {
        Ok(()) => tracing::info!(app = %app.name, job = %job_id, run = %run_id, "backup restored"),
        Err(err) => tracing::warn!(app = %app.name, job = %job_id, error = %err, "restore failed"),
    }
    result
}

async fn resolve_source(state: &AppState, job: &Job, app: &App) -> AppResult<BackupSource> {
    if job.driver == "volume" {
        let volume: String =
            sqlx::query_scalar("SELECT name FROM volumes WHERE id = ? AND app_id = ?")
                .bind(&job.volume_id)
                .bind(&app.id)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::validation("the job's volume no longer exists"))?;
        return Ok(BackupSource::Volume {
            volume_name: volume_name(&app.name, &volume),
        });
    }
    let container = if app.deploy_type == "compose" {
        let service = job
            .compose_service
            .clone()
            .or_else(|| app.compose_service.clone())
            .ok_or_else(|| AppError::validation("set compose_service on the job or the app"))?;
        compose_container_name(&app.name, &service)
    } else {
        app.active_container_id
            .clone()
            .ok_or_else(|| AppError::Conflict("app has no running container to dump".into()))?
    };
    let db_password = job
        .db_password_enc
        .as_ref()
        .map(|enc| {
            state
                .cipher
                .decrypt(enc, &aad("backup_jobs", &job.id, "db_password_enc"))
        })
        .transpose()?;
    let (db_name, db_user) = (job.db_name.clone(), job.db_user.clone());
    Ok(match job.driver.as_str() {
        "postgres" => BackupSource::Postgres {
            container,
            db_name,
            db_user,
            db_password,
        },
        "mysql" => BackupSource::Mysql {
            container,
            db_name,
            db_user,
            db_password,
        },
        other => return Err(AppError::validation(format!("unknown driver {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cron_parsing_and_matching() {
        assert!(parse_cron("0 2 * * *").is_ok());
        assert!(parse_cron("*/15 * * * 1-5").is_ok());
        for bad in [
            "",
            "0 2 * *",
            "0 0 2 * * *",
            "61 * * * *",
            "nonsense here x y z",
        ] {
            assert!(parse_cron(bad).is_err(), "{bad}");
        }
        let cron = parse_cron("30 2 * * *").unwrap();
        let at = |h, m| Utc.with_ymd_and_hms(2026, 11, 24, h, m, 0).unwrap();
        assert!(cron.is_time_matching(&at(2, 30)).unwrap());
        assert!(!cron.is_time_matching(&at(2, 31)).unwrap());
    }
}
