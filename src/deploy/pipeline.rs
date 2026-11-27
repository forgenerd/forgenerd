//! Zero-downtime deployment pipeline:
//! acquire image (pull, git build, or a previous image for rollback) → start candidate →
//! health gate → swap proxy routes → drain → remove old → prune old images.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::anyhow;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::routing;
use crate::api::AppState;
use crate::auth::crypto::aad;
use crate::db::models::{App, EnvVarRow, Volume};
use crate::db::new_id;
use crate::error::{AppError, AppResult};
use crate::executor::{
    BuildMethod, BuildSpec, ComposeSpec, ContainerSpec, HealthTarget, NodeExecutor, VolumeMount,
};

/// How long `docker compose up --wait` may take for a stack to become healthy.
const COMPOSE_WAIT_SECS: u64 = 180;

pub enum Trigger {
    /// Pull or build the app's current source (API deploy or git webhook).
    Build,
    /// Redeploy the image of a previous deployment without rebuilding.
    Rollback {
        image_ref: String,
        commit_sha: Option<String>,
    },
    /// Self-healing: recreate the active deployment's container from its image after it
    /// disappeared. Compose stacks (no image) are brought up again from their stored YAML.
    Recover {
        image_ref: Option<String>,
        commit_sha: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Build,
    Rollback,
    Recover,
}

/// Record a queued deployment and run it in the background. Returns the deployment id.
/// Build requests collapse into an already-queued build of the same app.
pub async fn enqueue(state: &AppState, app: &App, trigger: Trigger) -> AppResult<String> {
    match app.deploy_type.as_str() {
        "image" if app.image_name.as_deref().is_none_or(str::is_empty) => {
            return Err(AppError::validation("app has no image_name"));
        }
        "git" if app.git_repo_url.as_deref().is_none_or(str::is_empty) => {
            return Err(AppError::validation("app has no git_repo_url"));
        }
        "compose" => {}
        "image" | "git" => {}
        other => {
            return Err(AppError::validation(format!(
                "deploy_type '{other}' is not supported yet"
            )));
        }
    }

    let (image_ref, commit_sha, kind) = match trigger {
        Trigger::Build => {
            let queued: Option<String> = sqlx::query_scalar(
                "SELECT id FROM deployments WHERE app_id = ? AND status = 'queued' AND image_ref IS NULL LIMIT 1",
            )
            .bind(&app.id)
            .fetch_optional(&state.db)
            .await?;
            if let Some(id) = queued {
                return Ok(id);
            }
            (None, None, Kind::Build)
        }
        Trigger::Rollback {
            image_ref,
            commit_sha,
        } => (Some(image_ref), commit_sha, Kind::Rollback),
        Trigger::Recover {
            image_ref,
            commit_sha,
        } => {
            // Without an image, a non-compose app would be rebuilt from new source.
            if image_ref.is_none() && app.deploy_type != "compose" {
                return Err(AppError::validation(
                    "the active deployment has no image to recover from",
                ));
            }
            (image_ref, commit_sha, Kind::Recover)
        }
    };

    let id = new_id();
    let log_path = log_path(state, &id);
    sqlx::query(
        "INSERT INTO deployments (id, app_id, status, log_path, image_ref, commit_sha) \
         VALUES (?, ?, 'queued', ?, ?, ?)",
    )
    .bind(&id)
    .bind(&app.id)
    .bind(log_path.to_string_lossy().as_ref())
    .bind(image_ref)
    .bind(commit_sha)
    .execute(&state.db)
    .await?;

    let (state, app_id, deployment_id) = (state.clone(), app.id.clone(), id.clone());
    tokio::spawn(async move { run(state, app_id, deployment_id, kind).await });
    Ok(id)
}

pub fn log_path(state: &AppState, deployment_id: &str) -> PathBuf {
    state
        .data_dir
        .join("logs")
        .join(format!("{deployment_id}.log"))
}

async fn run(state: AppState, app_id: String, deployment_id: String, kind: Kind) {
    // One deployment per app at a time; later requests wait here as `queued`.
    let lock = state.deploy_locks.get(&app_id);
    let _guard = lock.lock().await;

    let (log, writer) = DeployLog::open(log_path(&state, &deployment_id)).await;
    let result = execute(&state, &app_id, &deployment_id, &log, kind).await;
    if let Err(err) = &result {
        log.line(format!("deployment failed: {err}")).await;
        tracing::warn!(app_id, deployment_id, error = %err, "deployment failed");
        // A failed recovery leaves nothing serving; otherwise the previous version is.
        let previous_serving = kind != Kind::Recover;
        if let Err(db_err) = mark_failed(&state, &app_id, &deployment_id, previous_serving).await {
            tracing::error!(error = %db_err, "recording failed deployment");
        }
    } else if kind == Kind::Recover {
        // Self-healing reported the app as failed when it found the container gone;
        // close that out. (A failed recovery is reported by the reconciler instead.)
        log.line("self-healing: the app is serving again".into())
            .await;
        notify_recovered(&state, &app_id).await;
    }
    drop(log);
    let _ = writer.await;
}

async fn execute(
    state: &AppState,
    app_id: &str,
    deployment_id: &str,
    log: &DeployLog,
    kind: Kind,
) -> AppResult<()> {
    // Reload inside the lock: config may have changed while this deployment was queued.
    let app = sqlx::query_as::<_, App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(app_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let executor = state.registry.get(&app.node_id)?;
    // A pre-set image means rollback: reuse it, no pull or build.
    let preset: Option<String> =
        sqlx::query_scalar("SELECT image_ref FROM deployments WHERE id = ?")
            .bind(deployment_id)
            .fetch_one(&state.db)
            .await?;

    sqlx::query("UPDATE deployments SET status = 'building' WHERE id = ?")
        .bind(deployment_id)
        .execute(&state.db)
        .await?;
    set_app_status(state, app_id, "building").await?;

    if kind == Kind::Recover {
        log.line("self-healing: the app's container no longer exists; recreating it".into())
            .await;
    }
    if app.deploy_type == "compose" {
        return execute_compose(state, &app, deployment_id, log, &*executor).await;
    }

    let rollback = kind == Kind::Rollback;
    let image_ref = match preset {
        Some(image) => {
            if rollback {
                log.line(format!("rolling back to image {image}")).await;
            } else {
                log.line(format!("reusing image {image}")).await;
            }
            image
        }
        None => {
            let spec = build_spec(state, &app, deployment_id).await?;
            let prepared = executor.prepare_image(&spec, log.sender()).await?;
            sqlx::query("UPDATE deployments SET image_ref = ?, commit_sha = ? WHERE id = ?")
                .bind(&prepared.image_ref)
                .bind(&prepared.commit_sha)
                .bind(deployment_id)
                .execute(&state.db)
                .await?;
            prepared.image_ref
        }
    };

    let spec = ContainerSpec {
        app_id: app.id.clone(),
        app_name: app.name.clone(),
        deployment_id: deployment_id.to_string(),
        image: image_ref,
        env: decrypt_env(state, app_id).await?,
        container_port: u16::try_from(app.container_port)
            .map_err(|_| AppError::validation("invalid container_port"))?,
        volumes: sqlx::query_as::<_, Volume>(
            "SELECT id, name, mount_path FROM volumes WHERE app_id = ?",
        )
        .bind(app_id)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|v| VolumeMount {
            volume_name: v.name,
            mount_path: v.mount_path,
        })
        .collect(),
        memory_limit_mb: app.memory_limit_mb.and_then(|mb| u32::try_from(mb).ok()),
        cpu_limit: app.cpu_limit,
    };
    log.line(format!("starting container for deployment {deployment_id}"))
        .await;
    let candidate = executor.start_container(&spec).await?;

    let target = HealthTarget {
        container_id: candidate.id.clone(),
        host: candidate.ip.clone(),
        port: spec.container_port,
        path: app.healthcheck_path.clone().unwrap_or_else(|| "/".into()),
    };
    log.line(format!(
        "waiting for {}:{}{} to become healthy",
        target.host, target.port, target.path
    ))
    .await;
    if let Err(err) = executor
        .wait_healthy(&target, app.health_timeout(state.timing.health_timeout))
        .await
    {
        log.line("health check failed; removing candidate, previous version keeps serving".into())
            .await;
        remove_logged(&*executor, &candidate.id, log).await;
        return Err(err);
    }

    // Stable internal hostname: only a healthy candidate answers on the app's name.
    if let Err(err) = executor.publish_alias(&candidate.id, &app.name).await {
        log.line("could not publish the internal hostname; removing candidate".into())
            .await;
        remove_logged(&*executor, &candidate.id, log).await;
        return Err(err);
    }
    log.line(format!(
        "reachable by other apps at {}:{}",
        app.name, app.container_port
    ))
    .await;

    // Atomic traffic swap: point the app at the candidate, then push the full route set.
    sqlx::query(
        "UPDATE apps SET active_container_id = ?, active_deployment_id = ?, status = 'running', \
         updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(&candidate.id)
    .bind(deployment_id)
    .bind(app_id)
    .execute(&state.db)
    .await?;
    if let Err(err) = routing::sync_node(state, &app.node_id).await {
        log.line("proxy update failed; reverting to the previous version".into())
            .await;
        sqlx::query(
            "UPDATE apps SET active_container_id = ?, active_deployment_id = ?, \
             updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(&app.active_container_id)
        .bind(&app.active_deployment_id)
        .bind(app_id)
        .execute(&state.db)
        .await?;
        remove_logged(&*executor, &candidate.id, log).await;
        return Err(err);
    }
    log.line("traffic switched to the new container".into())
        .await;

    if let Some(old) = app
        .active_container_id
        .as_deref()
        .filter(|old| *old != candidate.id)
    {
        log.line(format!(
            "draining old container for {}s",
            state.timing.drain.as_secs()
        ))
        .await;
        tokio::time::sleep(state.timing.drain).await;
        remove_logged(&*executor, old, log).await;
    }

    sqlx::query(
        "UPDATE deployments SET status = 'healthy', finished_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(deployment_id)
    .execute(&state.db)
    .await?;
    if rollback && let Some(previous) = &app.active_deployment_id {
        sqlx::query(
            "UPDATE deployments SET status = 'rolled_back' WHERE id = ? AND status = 'healthy'",
        )
        .bind(previous)
        .execute(&state.db)
        .await?;
    }
    log.line("deployment healthy".into()).await;

    // Keep the images of recent healthy deployments for rollback; drop the rest.
    if let Err(err) = crate::maintenance::prune_app(state, &executor, app_id, &app.name).await {
        log.line(format!("warning: image pruning failed: {err}"))
            .await;
    }
    Ok(())
}

/// Compose stacks are updated in place by `docker compose up` (not zero-downtime):
/// services whose config changed are recreated; `--wait` gates on their healthchecks.
async fn execute_compose(
    state: &AppState,
    app: &App,
    deployment_id: &str,
    log: &DeployLog,
    executor: &dyn NodeExecutor,
) -> AppResult<()> {
    let yaml: Option<String> = sqlx::query_scalar("SELECT compose_yaml FROM apps WHERE id = ?")
        .bind(&app.id)
        .fetch_one(&state.db)
        .await?;
    let spec = ComposeSpec {
        app_id: app.id.clone(),
        app_name: app.name.clone(),
        deployment_id: deployment_id.to_string(),
        compose_yaml: yaml.ok_or_else(|| AppError::validation("app has no compose_yaml"))?,
        env: decrypt_env(state, &app.id).await?,
        expose_service: app.compose_service.clone(),
        wait_timeout_secs: COMPOSE_WAIT_SECS,
    };
    let exposed = executor.deploy_compose(&spec, log.sender()).await?;

    // Stacks serving domains must also answer HTTP on the exposed service.
    let domains: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM domains WHERE app_id = ?")
        .bind(&app.id)
        .fetch_one(&state.db)
        .await?;
    if let Some(container) = &exposed
        && domains > 0
    {
        let target = HealthTarget {
            container_id: container.id.clone(),
            host: container.ip.clone(),
            port: u16::try_from(app.container_port)
                .map_err(|_| AppError::validation("invalid container_port"))?,
            path: app.healthcheck_path.clone().unwrap_or_else(|| "/".into()),
        };
        log.line(format!(
            "waiting for {}:{}{} to answer HTTP",
            target.host, target.port, target.path
        ))
        .await;
        executor
            .wait_healthy(&target, app.health_timeout(state.timing.health_timeout))
            .await?;
    }

    sqlx::query(
        "UPDATE apps SET active_container_id = ?, active_deployment_id = ?, status = 'running', \
         updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(exposed.as_ref().map(|c| c.id.clone()))
    .bind(deployment_id)
    .bind(&app.id)
    .execute(&state.db)
    .await?;
    routing::sync_node(state, &app.node_id).await?;
    sqlx::query(
        "UPDATE deployments SET status = 'healthy', finished_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(deployment_id)
    .execute(&state.db)
    .await?;
    log.line("compose stack healthy".into()).await;
    Ok(())
}

async fn build_spec(state: &AppState, app: &App, deployment_id: &str) -> AppResult<BuildSpec> {
    if app.deploy_type == "image" {
        let image = app
            .image_name
            .clone()
            .ok_or_else(|| AppError::validation("app has no image_name"))?;
        return Ok(BuildSpec::Pull { image });
    }
    let credential_enc: Option<String> =
        sqlx::query_scalar("SELECT git_credential_enc FROM apps WHERE id = ?")
            .bind(&app.id)
            .fetch_one(&state.db)
            .await?;
    let credential = credential_enc
        .map(|enc| {
            state
                .cipher
                .decrypt(&enc, &aad("apps", &app.id, "git_credential_enc"))
        })
        .transpose()?;
    Ok(BuildSpec::Git {
        app_id: app.id.clone(),
        app_name: app.name.clone(),
        deployment_id: deployment_id.to_string(),
        repo_url: app
            .git_repo_url
            .clone()
            .ok_or_else(|| AppError::validation("app has no git_repo_url"))?,
        branch: app.git_branch.clone().unwrap_or_else(|| "main".into()),
        credential,
        method: match app.build_method.as_deref() {
            Some("railpack") => BuildMethod::Railpack,
            _ => BuildMethod::Dockerfile,
        },
    })
}

async fn remove_logged(executor: &dyn crate::executor::NodeExecutor, id: &str, log: &DeployLog) {
    if let Err(err) = executor.remove_container(id).await {
        log.line(format!("warning: could not remove container {id}: {err}"))
            .await;
    }
}

pub async fn decrypt_env(state: &AppState, app_id: &str) -> AppResult<BTreeMap<String, String>> {
    let rows = sqlx::query_as::<_, EnvVarRow>(
        "SELECT id, key, value_enc, is_secret FROM env_vars WHERE app_id = ?",
    )
    .bind(app_id)
    .fetch_all(&state.db)
    .await?;
    rows.into_iter()
        .map(|row| {
            let value = state
                .cipher
                .decrypt(&row.value_enc, &aad("env_vars", &row.id, "value_enc"))
                .map_err(|e| anyhow!("env var {}: {e}", row.key))?;
            Ok((row.key, value))
        })
        .collect()
}

async fn set_app_status(state: &AppState, app_id: &str, status: &str) -> AppResult<()> {
    sqlx::query("UPDATE apps SET status = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(status)
        .bind(app_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn notify_recovered(state: &AppState, app_id: &str) {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM apps WHERE id = ?")
        .bind(app_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(app_name) = name else { return };
    crate::notify::dispatch(
        state.db.clone(),
        state.cipher.clone(),
        crate::notify::Event {
            kind: crate::notify::STATUS_CHANGED,
            app_id: app_id.to_string(),
            app_name,
            from: "failed".into(),
            to: "running".into(),
            detail: "recreated from the active deployment".into(),
        },
    );
}

async fn mark_failed(
    state: &AppState,
    app_id: &str,
    deployment_id: &str,
    previous_serving: bool,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE deployments SET status = 'failed', finished_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(deployment_id)
    .execute(&state.db)
    .await?;
    // The previous version (if any) is still serving, unless this was a recovery.
    sqlx::query(
        "UPDATE apps SET status = CASE WHEN ? AND active_deployment_id IS NOT NULL \
         THEN 'running' ELSE 'failed' END, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(previous_serving)
    .bind(app_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Appends timestamped lines to the deployment's log file via a background writer.
struct DeployLog {
    tx: mpsc::Sender<String>,
}

impl DeployLog {
    async fn open(path: PathBuf) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let writer = tokio::spawn(async move {
            let file = async {
                if let Some(dir) = path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
            };
            let mut file = match file.await {
                Ok(f) => Some(f),
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "cannot open deployment log");
                    None
                }
            };
            while let Some(line) = rx.recv().await {
                if let Some(f) = file.as_mut() {
                    let stamped = format!("{} {line}\n", chrono::Utc::now().format("%H:%M:%S"));
                    if f.write_all(stamped.as_bytes()).await.is_err() {
                        file = None;
                    }
                }
            }
        });
        (Self { tx }, writer)
    }

    fn sender(&self) -> mpsc::Sender<String> {
        self.tx.clone()
    }

    async fn line(&self, line: String) {
        let _ = self.tx.send(line).await;
    }
}
