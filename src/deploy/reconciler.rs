//! Background self-healing. Docker's `unless-stopped`
//! policy restarts crashed containers; this loop handles what Docker does not:
//! - a running container that fails its HTTP healthcheck (or that Docker's own
//!   HEALTHCHECK reports unhealthy) `FAILURE_THRESHOLD` times in a row is restarted;
//! - a container stopped outside forgenerd is started again;
//! - a container that no longer exists is recreated from the active deployment's image
//!   (compose stacks: brought up again) through the deployment pipeline's health gate.
//!
//! Actions back off exponentially; after `MAX_ATTEMPTS` without the app staying healthy
//! for `heal_stable`, healing stops and the app is marked `failed` until a person acts.
//! The loop also updates `apps.status`, sends notifications, and re-asserts the proxy
//! config.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::{pipeline, routing};
use crate::api::AppState;
use crate::db::models::App;
use crate::error::{AppError, AppResult};
use crate::executor::{ContainerAction, ContainerHealth, HealthTarget, NodeExecutor};
use crate::notify::{self, Event};

pub const INTERVAL: Duration = Duration::from_secs(15);
/// Consecutive failed checks of a running container before it is restarted.
const FAILURE_THRESHOLD: u32 = 3;
/// Healing actions on one app before giving up.
const MAX_ATTEMPTS: u32 = 5;
/// Budget for one HTTP probe of a running container.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// What the reconciler remembers about one node between ticks.
#[derive(Default)]
pub struct Memory {
    /// Last seen restart count per container (crash-loop detection).
    restarts: HashMap<String, i64>,
    /// Self-healing progress per app.
    heal: HashMap<String, Heal>,
}

#[derive(Default)]
struct Heal {
    /// Deployment this progress belongs to; a new deployment starts over.
    deployment_id: String,
    /// Consecutive failed checks of a running container.
    failures: u32,
    /// Actions taken since the app was last stably healthy.
    attempts: u32,
    not_before: Option<Instant>,
    healthy_since: Option<Instant>,
    gave_up: bool,
}

/// Once at startup, before serving traffic: remove managed containers that belong to no
/// app's active deployment (candidates orphaned by a crash mid-deploy).
pub async fn remove_orphans(state: &AppState) -> AppResult<usize> {
    let active: HashSet<String> = sqlx::query_scalar(
        "SELECT active_deployment_id FROM apps WHERE active_deployment_id IS NOT NULL",
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();
    let apps: HashSet<String> = sqlx::query_scalar("SELECT id FROM apps")
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect();
    let mut removed = 0;
    for node_id in state.registry.ids() {
        let executor = state.registry.get(&node_id)?;
        for container in executor.list_managed_containers().await? {
            // Compose containers (no deployment id) belong to their app's stack; they are
            // orphans only if the app itself is gone.
            let orphan = if container.deployment_id.is_empty() {
                !apps.contains(&container.app_id)
            } else {
                !active.contains(&container.deployment_id)
            };
            if orphan {
                tracing::info!(container = %container.id, app = %container.app_id, "removing orphaned container");
                executor.remove_container(&container.id).await?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut memory: HashMap<String, Memory> = HashMap::new();
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for node_id in state.registry.ids() {
                let node_memory = memory.entry(node_id.clone()).or_default();
                if let Err(err) = reconcile_node(&state, &node_id, node_memory).await {
                    tracing::warn!(node = %node_id, error = %err, "reconcile failed");
                }
            }
        }
    });
}

#[derive(sqlx::FromRow)]
struct AppRow {
    id: String,
    name: String,
    status: String,
    deploy_type: String,
    active_container_id: Option<String>,
    active_deployment_id: String,
    container_port: i64,
    healthcheck_path: Option<String>,
}

/// What a tick observed about an app.
enum Finding {
    Healthy,
    /// Broken in a way the reconciler leaves alone (Docker is restarting a crash loop,
    /// or a compose service exited, which may be a finished one-shot job).
    Degraded(String),
    /// A container that exists but is not running.
    Stopped(String),
    /// Running containers failing their checks.
    Unresponsive(Vec<String>, String),
    /// No container of the app exists.
    Missing,
}

enum Action {
    Start(String),
    Restart(Vec<String>),
    Recreate,
}

impl Action {
    fn describe(&self) -> &'static str {
        match self {
            Self::Start(_) => "starting it again",
            Self::Restart(_) => "restarting it",
            Self::Recreate => "recreating it from the active deployment",
        }
    }
}

pub async fn reconcile_node(state: &AppState, node_id: &str, memory: &mut Memory) -> AppResult<()> {
    let executor = state.registry.get(node_id)?;
    // Read apps BEFORE listing containers: any active container recorded here already
    // existed when the listing ran, so a concurrent deploy cannot look like a missing
    // container. Only apps meant to be serving are checked; stopped/building are left alone.
    let apps = sqlx::query_as::<_, AppRow>(
        "SELECT id, name, status, deploy_type, active_container_id, active_deployment_id, \
         container_port, healthcheck_path FROM apps \
         WHERE node_id = ? AND active_deployment_id IS NOT NULL \
         AND status IN ('running', 'unhealthy', 'failed')",
    )
    .bind(node_id)
    .fetch_all(&state.db)
    .await?;

    let containers = match executor.list_managed_containers().await {
        Ok(c) => c,
        Err(err) => {
            set_node_status(state, node_id, "offline").await?;
            return Err(err);
        }
    };
    set_node_status(state, node_id, "online").await?;
    let by_id: HashMap<&str, &ContainerHealth> =
        containers.iter().map(|c| (c.id.as_str(), c)).collect();

    // Image/git apps: their active container. Compose apps: every container of the stack.
    let members = |app: &AppRow| -> Vec<&ContainerHealth> {
        if app.deploy_type == "compose" {
            containers.iter().filter(|c| c.app_id == app.id).collect()
        } else {
            app.active_container_id
                .as_deref()
                .and_then(|id| by_id.get(id).copied())
                .into_iter()
                .collect()
        }
    };

    // HTTP-probe every running image/git container concurrently: a hung process keeps
    // its container "running", so Docker alone never notices.
    let probes = apps.iter().filter_map(|app| {
        let c = members(app).into_iter().next()?;
        if app.deploy_type == "compose" || !c.running || c.restarting {
            return None;
        }
        let target = HealthTarget {
            container_id: c.id.clone(),
            host: c.ip.clone()?,
            port: u16::try_from(app.container_port).ok()?,
            path: app.healthcheck_path.clone().unwrap_or_else(|| "/".into()),
        };
        let (executor, app_id) = (executor.clone(), app.id.clone());
        Some(async move {
            let result = executor.wait_healthy(&target, PROBE_TIMEOUT).await;
            let failure = result
                .err()
                .map(|err| format!("not answering HTTP on {}: {err:#}", target.path));
            (app_id, failure)
        })
    });
    let probe_failures: HashMap<String, Option<String>> = futures_util::future::join_all(probes)
        .await
        .into_iter()
        .collect();

    let now = Instant::now();
    for app in &apps {
        let probe_failure = probe_failures.get(&app.id).cloned().flatten();
        let finding = assess(app, &members(app), probe_failure, &mut memory.restarts);

        let heal = memory.heal.entry(app.id.clone()).or_default();
        // A new deployment, or someone reviving an app the reconciler gave up on,
        // starts healing over.
        if heal.deployment_id != app.active_deployment_id
            || (heal.gave_up && app.status != "failed")
        {
            *heal = Heal {
                deployment_id: app.active_deployment_id.clone(),
                ..Heal::default()
            };
        }
        if !matches!(finding, Finding::Healthy) {
            heal.healthy_since = None;
        }

        let (mut status, mut detail, mut action): (&str, String, Option<Action>) = match finding {
            Finding::Healthy => {
                heal.failures = 0;
                let since = *heal.healthy_since.get_or_insert(now);
                if now.duration_since(since) >= state.timing.heal_stable {
                    heal.attempts = 0;
                    heal.not_before = None;
                }
                ("running", "healthy again".into(), None)
            }
            Finding::Degraded(detail) => ("unhealthy", detail, None),
            Finding::Stopped(id) => (
                "unhealthy",
                "container is not running".into(),
                Some(Action::Start(id)),
            ),
            Finding::Unresponsive(ids, detail) => {
                heal.failures += 1;
                if heal.failures < FAILURE_THRESHOLD {
                    // Possibly a blip: keep the current status until it persists.
                    (app.status.as_str(), detail, None)
                } else {
                    ("unhealthy", detail, Some(Action::Restart(ids)))
                }
            }
            Finding::Missing => (
                "failed",
                "container no longer exists".into(),
                Some(Action::Recreate),
            ),
        };

        let mut gave_up_now = false;
        if let Some(planned) = &action {
            if heal.gave_up {
                status = "failed";
                action = None;
            } else if heal.attempts >= MAX_ATTEMPTS {
                heal.gave_up = true;
                gave_up_now = true;
                status = "failed";
                detail = format!("self-healing gave up after {MAX_ATTEMPTS} attempts: {detail}");
                action = None;
            } else if heal.not_before.is_some_and(|t| now < t) {
                action = None;
            } else {
                detail = format!("{detail}; {}", planned.describe());
            }
        }

        if status != app.status || gave_up_now {
            set_status(state, app, status, detail).await?;
        }

        if let Some(action) = action {
            match heal_app(state, &*executor, app, &action).await {
                Ok(false) => {}
                outcome => {
                    // Failed actions count too, so a persistent failure ends in giving up.
                    if let Err(err) = outcome {
                        tracing::warn!(app = %app.name, error = %err, "self-healing action failed");
                    } else {
                        tracing::warn!(app = %app.name, action = action.describe(), "self-healing");
                    }
                    heal.attempts += 1;
                    heal.failures = 0;
                    heal.not_before = Some(now + backoff(state.timing.heal_backoff, heal.attempts));
                }
            }
        }
    }
    memory
        .restarts
        .retain(|id, _| by_id.contains_key(id.as_str()));
    let known: HashSet<String> = sqlx::query_scalar("SELECT id FROM apps WHERE node_id = ?")
        .bind(node_id)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect();
    memory.heal.retain(|id, _| known.contains(id));

    // Re-assert the proxy config (recovers from Caddy restarts; unchanged configs are
    // a no-op in Caddy).
    routing::sync_node(state, node_id).await
}

/// Worst state across an app's containers. Updates the restart-count memory.
fn assess(
    app: &AppRow,
    members: &[&ContainerHealth],
    probe_failure: Option<String>,
    restarts: &mut HashMap<String, i64>,
) -> Finding {
    if members.is_empty() {
        return Finding::Missing;
    }
    let mut crash = None;
    let mut stopped = None;
    let mut unresponsive = Vec::new();
    for c in members {
        let previous = restarts.insert(c.id.clone(), c.restart_count);
        let restarted = previous.is_some_and(|p| c.restart_count > p);
        if c.restarting || restarted {
            crash.get_or_insert_with(|| format!("crash loop (restart count {})", c.restart_count));
        } else if !c.running {
            stopped.get_or_insert_with(|| c.id.clone());
        } else if c.health.as_deref() == Some("unhealthy") {
            unresponsive.push(c.id.clone());
        }
    }
    if let Some(detail) = crash {
        return Finding::Degraded(detail);
    }
    if let Some(id) = stopped {
        return if app.deploy_type == "compose" {
            Finding::Degraded("container is not running".into())
        } else {
            Finding::Stopped(id)
        };
    }
    if !unresponsive.is_empty() {
        return Finding::Unresponsive(unresponsive, "Docker healthcheck reports unhealthy".into());
    }
    match probe_failure {
        Some(detail) => Finding::Unresponsive(vec![members[0].id.clone()], detail),
        None => Finding::Healthy,
    }
}

/// `base`, doubled per previous attempt, capped at `MAX_BACKOFF`.
fn backoff(base: Duration, attempts: u32) -> Duration {
    base.saturating_mul(1 << attempts.saturating_sub(1).min(16))
        .min(MAX_BACKOFF)
}

/// Performs a healing action. Returns `Ok(false)` without acting when a deployment is
/// running or queued, or the app changed since this tick read it.
async fn heal_app(
    state: &AppState,
    executor: &dyn NodeExecutor,
    app: &AppRow,
    action: &Action,
) -> AppResult<bool> {
    // Deployments hold this lock while they swap containers; never act during one.
    let lock = state.deploy_locks.get(&app.id);
    let Ok(_deploy_guard) = lock.try_lock() else {
        return Ok(false);
    };
    // Shared with the panel's start/stop/restart: the re-check below and the action are
    // atomic with respect to a person stopping the app.
    let container_lock = state.container_locks.get(&app.id);
    let _container_guard = container_lock.lock().await;
    let current: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT status, active_container_id, active_deployment_id FROM apps WHERE id = ?",
    )
    .bind(&app.id)
    .fetch_optional(&state.db)
    .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployments WHERE app_id = ? AND status IN ('queued', 'building')",
    )
    .bind(&app.id)
    .fetch_one(&state.db)
    .await?;
    let unchanged = current.is_some_and(|(status, container, deployment)| {
        ["running", "unhealthy", "failed"].contains(&status.as_str())
            && container == app.active_container_id
            && deployment.as_deref() == Some(app.active_deployment_id.as_str())
    });
    if !unchanged || pending > 0 {
        return Ok(false);
    }

    match action {
        Action::Start(id) => {
            executor
                .set_container_state(id, ContainerAction::Start)
                .await?;
        }
        Action::Restart(ids) => {
            for id in ids {
                executor
                    .set_container_state(id, ContainerAction::Restart)
                    .await?;
            }
        }
        Action::Recreate => {
            let full = sqlx::query_as::<_, App>(concat!(
                "SELECT ",
                crate::app_columns!(),
                " FROM apps WHERE id = ?"
            ))
            .bind(&app.id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
            let (image_ref, commit_sha): (Option<String>, Option<String>) =
                sqlx::query_as("SELECT image_ref, commit_sha FROM deployments WHERE id = ?")
                    .bind(&app.active_deployment_id)
                    .fetch_optional(&state.db)
                    .await?
                    .unwrap_or_default();
            // The spawned deployment waits for the lock held here until this returns.
            pipeline::enqueue(
                state,
                &full,
                pipeline::Trigger::Recover {
                    image_ref,
                    commit_sha,
                },
            )
            .await?;
        }
    }
    Ok(true)
}

/// Records a status change and notifies. The update is conditional: a deployment that
/// changed the status or swapped the container meanwhile wins.
async fn set_status(state: &AppState, app: &AppRow, status: &str, detail: String) -> AppResult<()> {
    let changed = sqlx::query(
        "UPDATE apps SET status = ?, updated_at = CURRENT_TIMESTAMP \
         WHERE id = ? AND status = ? AND active_deployment_id = ?",
    )
    .bind(status)
    .bind(&app.id)
    .bind(&app.status)
    .bind(&app.active_deployment_id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if changed == 1 {
        tracing::warn!(app = %app.name, from = %app.status, to = status, %detail, "app status changed");
        notify::dispatch(
            state.db.clone(),
            state.cipher.clone(),
            Event {
                kind: notify::STATUS_CHANGED,
                app_id: app.id.clone(),
                app_name: app.name.clone(),
                from: app.status.clone(),
                to: status.to_string(),
                detail,
            },
        );
    }
    Ok(())
}

async fn set_node_status(state: &AppState, node_id: &str, status: &str) -> AppResult<()> {
    sqlx::query("UPDATE nodes SET status = ?, last_seen_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(status)
        .bind(node_id)
        .execute(&state.db)
        .await?;
    Ok(())
}
