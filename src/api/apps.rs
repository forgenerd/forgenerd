use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::AppState;
use crate::auth::AuthUser;
use crate::auth::crypto::aad;
use crate::db::models::{App, Deployment, Domain, EnvVarRow, Volume};
use crate::db::new_id;
use crate::deploy::routing::PublicNames;
use crate::deploy::{pipeline, routing};
use crate::error::{AppError, AppResult};
use crate::executor::{
    ContainerAction, ContainerHealth, compose_container_name, compose_project, volume_name,
};
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", get(show).patch(update).delete(remove))
        .route("/{id}/deployment", post(configure_deployment))
        .route("/{id}/deploy", post(deploy))
        .route("/{id}/webhook", get(webhook_info))
        .route("/{id}/compose", get(compose_file))
        .route("/{id}/services", get(list_services))
        .route("/{id}/services/{service}", patch(update_service))
        .route("/{id}/services/{service}/logs", get(service_logs))
        .route("/{id}/services/{service}/{action}", post(service_control))
        .route("/{id}/certificates", get(certificates))
        .route("/{id}/stats", get(stats))
        .route("/{id}/logs", get(container_logs))
        .route("/{id}/{action}", post(control))
}

async fn fetch_app(state: &AppState, id: &str) -> AppResult<App> {
    sqlx::query_as::<_, App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// An app as shown on the status board: its public hostnames and latest deployment.
#[derive(Serialize)]
struct AppSummary {
    #[serde(flatten)]
    app: App,
    /// Hostnames the app is served on (default domain first); empty when not exposed.
    domains: Vec<String>,
    last_deployment: Option<Deployment>,
}

async fn list(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<Vec<AppSummary>>> {
    let apps = sqlx::query_as::<_, App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps ORDER BY name"
    ))
    .fetch_all(&state.db)
    .await?;
    let domains: Vec<(String, String)> =
        sqlx::query_as("SELECT app_id, hostname FROM domains ORDER BY hostname")
            .fetch_all(&state.db)
            .await?;
    // Latest deployment per app (window function keeps it to one query).
    let latest = sqlx::query_as::<_, Deployment>(concat!(
        "SELECT ",
        crate::deployment_columns!(),
        " FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY app_id ORDER BY started_at DESC, id DESC) AS rn \
         FROM deployments) WHERE rn = 1"
    ))
    .fetch_all(&state.db)
    .await?;
    let mut by_app: std::collections::HashMap<String, Deployment> =
        latest.into_iter().map(|d| (d.app_id.clone(), d)).collect();
    let names = PublicNames::load(&state.db).await?;
    Ok(Json(
        apps.into_iter()
            .map(|app| AppSummary {
                domains: names.hostnames(
                    &app.name,
                    app.exposed,
                    &domains
                        .iter()
                        .filter(|(id, _)| *id == app.id)
                        .map(|(_, h)| h.clone())
                        .collect::<Vec<_>>(),
                ),
                last_deployment: by_app.remove(&app.id),
                app,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateApp {
    project_id: String,
    name: String,
    #[serde(default = "default_deploy_type")]
    deploy_type: String,
    image_name: Option<String>,
    git_repo_url: Option<String>,
    git_branch: Option<String>,
    build_method: Option<String>,
    /// Access token (https) or private deploy key (ssh); stored encrypted.
    git_credential: Option<String>,
    compose_yaml: Option<String>,
    /// Compose service reachable by Caddy and other apps (alias = app name).
    compose_service: Option<String>,
    container_port: Option<i64>,
    healthcheck_path: Option<String>,
    /// Named Docker volumes to create with the app.
    volumes: Option<Vec<VolumeInput>>,
    /// Serve the app publicly (default). Without a root domain or custom domain there
    /// is nothing to serve it on yet.
    #[serde(default = "yes")]
    exposed: bool,
}

fn default_deploy_type() -> String {
    "draft".into()
}

fn validate_build_method(method: &str) -> AppResult<()> {
    if matches!(method, "dockerfile" | "railpack") {
        Ok(())
    } else {
        Err(AppError::validation(
            "build_method must be \"dockerfile\" or \"railpack\"",
        ))
    }
}

/// Insert or update one encrypted env var. `existing` is the row id if the key exists:
/// the AAD binds the ciphertext to the row id, so an existing id must be reused.
pub(super) async fn write_env(
    tx: &mut sqlx::SqliteConnection,
    state: &AppState,
    app_id: &str,
    existing: Option<String>,
    key: &str,
    value: &str,
    is_secret: bool,
) -> AppResult<()> {
    let row_id = existing.unwrap_or_else(new_id);
    let enc = state
        .cipher
        .encrypt(value, &aad("env_vars", &row_id, "value_enc"))?;
    sqlx::query(
        "INSERT INTO env_vars (id, app_id, key, value_enc, is_secret) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET value_enc = excluded.value_enc, is_secret = excluded.is_secret",
    )
    .bind(&row_id)
    .bind(app_id)
    .bind(key)
    .bind(enc)
    .bind(is_secret)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

pub(super) fn check_compose_service(service: Option<&str>, services: &[String]) -> AppResult<()> {
    match service {
        Some(s) if !services.iter().any(|x| x == s) => Err(AppError::validation(format!(
            "compose_service `{s}` is not a service in compose_yaml"
        ))),
        _ => Ok(()),
    }
}

fn validate_credential(credential: &str) -> AppResult<()> {
    if credential.is_empty() || credential.len() > 16 * 1024 || credential.contains('\0') {
        Err(AppError::validation(
            "git_credential must be 1-16384 characters",
        ))
    } else {
        Ok(())
    }
}

async fn create(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateApp>,
) -> AppResult<(StatusCode, Json<AppDetail>)> {
    let name = req.name.trim().to_ascii_lowercase();
    validate::app_name(&name)?;
    let port = req.container_port.unwrap_or(8080);
    validate::port(port)?;
    let health = req.healthcheck_path.unwrap_or_else(|| "/".into());
    validate::http_path(&health)?;

    let id = new_id();
    // Preserve the legacy API shorthand where the source field alone selected the
    // deployment method, while allowing an empty request to create a draft app.
    let deploy_type = if req.deploy_type == "draft" {
        if req.image_name.is_some() {
            "image"
        } else if req.git_repo_url.is_some() {
            "git"
        } else if req.compose_yaml.is_some() {
            "compose"
        } else {
            "draft"
        }
    } else {
        req.deploy_type.as_str()
    };
    let mut compose: (Option<String>, Option<String>) = (None, None);
    let (image, repo, branch, method, credential_enc, webhook_secret_enc) = match deploy_type {
        "draft" => (None, None, None, None, None, None),
        "compose" => {
            let yaml = req
                .compose_yaml
                .as_deref()
                .ok_or_else(|| AppError::validation("compose_yaml is required"))?;
            let services = validate::compose::check_raw(yaml)?;
            check_compose_service(req.compose_service.as_deref(), &services)?;
            compose = (Some(yaml.to_string()), req.compose_service.clone());
            (None, None, None, None, None, None)
        }
        "image" => {
            let image = req
                .image_name
                .as_deref()
                .map(str::trim)
                .ok_or_else(|| AppError::validation("image_name is required"))?;
            validate::image_ref(image)?;
            (Some(image.to_string()), None, None, None, None, None)
        }
        "git" => {
            let repo = req
                .git_repo_url
                .as_deref()
                .map(str::trim)
                .ok_or_else(|| AppError::validation("git_repo_url is required"))?;
            validate::git_url(repo)?;
            let branch = req.git_branch.as_deref().map_or("main", str::trim);
            validate::git_branch(branch)?;
            let method = req.build_method.as_deref().unwrap_or("dockerfile");
            validate_build_method(method)?;
            let credential = match req.git_credential.as_deref() {
                Some(c) => {
                    validate_credential(c)?;
                    Some(
                        state
                            .cipher
                            .encrypt(c, &aad("apps", &id, "git_credential_enc"))?,
                    )
                }
                None => None,
            };
            let secret = state.cipher.encrypt(
                &crate::auth::new_token(),
                &aad("apps", &id, "webhook_secret_enc"),
            )?;
            (
                None,
                Some(repo.to_string()),
                Some(branch.to_string()),
                Some(method.to_string()),
                credential,
                Some(secret),
            )
        }
        _ => {
            return Err(AppError::validation(
                "deploy_type must be \"draft\", \"image\", \"git\" or \"compose\"",
            ));
        }
    };

    let project: Option<String> = sqlx::query_scalar("SELECT id FROM projects WHERE id = ?")
        .bind(&req.project_id)
        .fetch_optional(&state.db)
        .await?;
    if project.is_none() {
        return Err(AppError::validation("project_id does not exist"));
    }
    if deploy_type == "compose" && req.volumes.is_some() {
        return Err(AppError::validation(
            "compose apps declare volumes in compose_yaml",
        ));
    }
    if let Some(volumes) = &req.volumes {
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for volume in volumes {
            validate::name(&volume.name)?;
            validate::mount_path(&volume.mount_path)?;
            if !names.insert(&volume.name) || !paths.insert(&volume.mount_path) {
                return Err(AppError::validation(
                    "volume names and mount paths must be unique",
                ));
            }
        }
    }
    check_default_domain_free(&state, &name).await?;

    sqlx::query(
        "INSERT INTO apps (id, project_id, name, deploy_type, image_name, git_repo_url, git_branch, \
         build_method, git_credential_enc, webhook_secret_enc, compose_yaml, compose_service, \
         container_port, healthcheck_path, exposed) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.project_id)
    .bind(&name)
    .bind(deploy_type)
    .bind(image)
    .bind(repo)
    .bind(branch)
    .bind(method)
    .bind(credential_enc)
    .bind(webhook_secret_enc)
    .bind(compose.0)
    .bind(compose.1)
    .bind(port)
    .bind(&health)
    .bind(req.exposed)
    .execute(&state.db)
    .await?;
    if let Some(volumes) = &req.volumes {
        for volume in volumes {
            sqlx::query("INSERT INTO volumes (id, app_id, name, mount_path) VALUES (?, ?, ?, ?)")
                .bind(new_id())
                .bind(&id)
                .bind(&volume.name)
                .bind(&volume.mount_path)
                .execute(&state.db)
                .await?;
        }
    }
    Ok((StatusCode::CREATED, Json(detail(&state, &id).await?)))
}

#[derive(Deserialize)]
struct DeploymentConfig {
    deploy_type: String,
    image_name: Option<String>,
    git_repo_url: Option<String>,
    git_branch: Option<String>,
    build_method: Option<String>,
    git_credential: Option<String>,
    compose_yaml: Option<String>,
    compose_service: Option<String>,
}

/// Configure or switch the source used by the next deployment. Keeping this separate from
/// the general app settings lets a newly-created draft remain source-free until the user
/// chooses a deployment method in the Deployments tab.
async fn configure_deployment(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DeploymentConfig>,
) -> AppResult<Json<UpdateResponse>> {
    let app = fetch_app(&state, &id).await?;
    let target = req.deploy_type.as_str();
    if !matches!(target, "draft" | "image" | "git" | "compose") {
        return Err(AppError::validation(
            "deploy_type must be \"draft\", \"image\", \"git\" or \"compose\"",
        ));
    }

    let image = if target == "image" {
        let image = req
            .image_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError::validation("image_name is required"))?;
        validate::image_ref(image)?;
        Some(image.to_string())
    } else {
        None
    };

    let (repo, branch, method, credential_enc, webhook_secret_enc) = if target == "git" {
        let repo = req
            .git_repo_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError::validation("git_repo_url is required"))?;
        validate::git_url(repo)?;
        let branch = req
            .git_branch
            .as_deref()
            .unwrap_or("main")
            .trim()
            .to_string();
        validate::git_branch(&branch)?;
        let method = req
            .build_method
            .as_deref()
            .unwrap_or("dockerfile")
            .to_string();
        validate_build_method(&method)?;
        let credential_enc = match req
            .git_credential
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some(credential) => {
                validate_credential(credential)?;
                Some(
                    state
                        .cipher
                        .encrypt(credential, &aad("apps", &id, "git_credential_enc"))?,
                )
            }
            None if app.deploy_type == "git" => {
                sqlx::query_scalar("SELECT git_credential_enc FROM apps WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&state.db)
                    .await?
            }
            None => None,
        };
        let webhook_secret_enc = state.cipher.encrypt(
            &crate::auth::new_token(),
            &aad("apps", &id, "webhook_secret_enc"),
        )?;
        (
            Some(repo.to_string()),
            Some(branch),
            Some(method),
            credential_enc,
            Some(webhook_secret_enc),
        )
    } else {
        (None, None, None, None, None)
    };

    let (compose_yaml, compose_service) = if target == "compose" {
        let yaml = req
            .compose_yaml
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AppError::validation("compose_yaml is required"))?;
        let services = validate::compose::check_raw(yaml)?;
        let service = req
            .compose_service
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        check_compose_service(service, &services)?;
        (Some(yaml.to_string()), service.map(str::to_string))
    } else {
        (None, None)
    };

    sqlx::query(
        "UPDATE apps SET deploy_type = ?, image_name = ?, git_repo_url = ?, git_branch = ?, \
         build_method = ?, git_credential_enc = ?, webhook_secret_enc = ?, compose_yaml = ?, \
         compose_service = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(target)
    .bind(image)
    .bind(repo)
    .bind(branch)
    .bind(method)
    .bind(credential_enc)
    .bind(webhook_secret_enc)
    .bind(compose_yaml)
    .bind(compose_service)
    .bind(&id)
    .execute(&state.db)
    .await?;

    Ok(Json(UpdateResponse {
        detail: detail(&state, &id).await?,
        redeploy_required: app.active_deployment_id.is_some(),
    }))
}

/// A new app named `name` would default to `<name>.<root>`: refuse when the panel
/// already uses that hostname (an app domain would silently take precedence otherwise).
pub(super) async fn check_default_domain_free(state: &AppState, name: &str) -> AppResult<()> {
    let candidate = PublicNames::load(&state.db).await?.candidate(name);
    let panel = routing::panel_domain(&state.db).await?;
    match candidate {
        Some(h) if panel.as_deref() == Some(h.as_str()) => Err(AppError::Conflict(format!(
            "{h} is the panel domain; choose another app name"
        ))),
        _ => Ok(()),
    }
}

#[derive(Serialize)]
struct EnvVarView {
    key: String,
    /// `null` for secrets: values are never returned once stored.
    value: Option<String>,
    is_secret: bool,
}

#[derive(Serialize)]
pub(super) struct AppDetail {
    #[serde(flatten)]
    pub(super) app: App,
    /// `<name>.<root_domain>` when a root domain is set and nothing else claims it.
    /// Served only while the app is exposed.
    default_domain: Option<String>,
    /// Custom domains, in addition to the default one.
    domains: Vec<Domain>,
    env: Vec<EnvVarView>,
    volumes: Vec<Volume>,
    deployments: Vec<Deployment>,
}

pub(super) async fn detail(state: &AppState, id: &str) -> AppResult<AppDetail> {
    let app = fetch_app(state, id).await?;
    let domains = sqlx::query_as::<_, Domain>(
        "SELECT id, hostname, force_https, created_at FROM domains WHERE app_id = ? ORDER BY hostname",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let env = sqlx::query_as::<_, EnvVarRow>(
        "SELECT id, key, value_enc, is_secret FROM env_vars WHERE app_id = ? ORDER BY key",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|row| {
        let value = if row.is_secret {
            None
        } else {
            Some(
                state
                    .cipher
                    .decrypt(&row.value_enc, &aad("env_vars", &row.id, "value_enc"))?,
            )
        };
        Ok(EnvVarView {
            key: row.key,
            value,
            is_secret: row.is_secret,
        })
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
    let volumes = sqlx::query_as::<_, Volume>(
        "SELECT id, name, mount_path FROM volumes WHERE app_id = ? ORDER BY name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let deployments = sqlx::query_as::<_, Deployment>(concat!(
        "SELECT ",
        crate::deployment_columns!(),
        " FROM deployments WHERE app_id = ? ORDER BY started_at DESC, id DESC LIMIT 10"
    ))
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let default_domain = PublicNames::load(&state.db).await?.default_for(&app.name);
    Ok(AppDetail {
        app,
        default_domain,
        domains,
        env,
        volumes,
        deployments,
    })
}

async fn show(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<AppDetail>> {
    Ok(Json(detail(&state, &id).await?))
}

#[derive(Deserialize)]
struct EnvInput {
    value: String,
    #[serde(default = "yes")]
    is_secret: bool,
}

fn yes() -> bool {
    true
}

/// Distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct DomainInput {
    hostname: String,
    #[serde(default = "yes")]
    force_https: bool,
}

#[derive(Deserialize)]
struct VolumeInput {
    name: String,
    mount_path: String,
}

/// Absent fields are unchanged. `env` upserts keys (`null` deletes a key); `domains` and
/// `volumes` replace the whole set.
#[derive(Deserialize)]
struct UpdateApp {
    image_name: Option<String>,
    git_repo_url: Option<String>,
    git_branch: Option<String>,
    build_method: Option<String>,
    /// `null` removes the stored credential.
    #[serde(default, deserialize_with = "present")]
    git_credential: Option<Option<String>>,
    compose_yaml: Option<String>,
    /// `null` keeps the whole stack private.
    #[serde(default, deserialize_with = "present")]
    compose_service: Option<Option<String>>,
    container_port: Option<i64>,
    healthcheck_path: Option<String>,
    env: Option<BTreeMap<String, Option<EnvInput>>>,
    domains: Option<Vec<DomainInput>>,
    volumes: Option<Vec<VolumeInput>>,
    /// Serve the app publicly; `false` removes every public route, keeping the domains.
    exposed: Option<bool>,
    /// `null` removes the limit. Image and git apps only; compose apps set limits in
    /// their compose file.
    #[serde(default, deserialize_with = "present")]
    memory_limit_mb: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    cpu_limit: Option<Option<f64>>,
    /// `null` restores the default.
    #[serde(default, deserialize_with = "present")]
    health_timeout_secs: Option<Option<i64>>,
}

#[derive(Serialize)]
struct UpdateResponse {
    #[serde(flatten)]
    detail: AppDetail,
    /// Container config changed: the running version keeps the old config until redeployed.
    redeploy_required: bool,
}

async fn update(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateApp>,
) -> AppResult<Json<UpdateResponse>> {
    let app = fetch_app(&state, &id).await?;

    // Validate everything before writing anything.
    let image = req.image_name.as_deref().map(str::trim);
    if let Some(image) = image {
        if app.deploy_type != "image" {
            return Err(AppError::validation(
                "image_name only applies to image apps",
            ));
        }
        validate::image_ref(image)?;
    }
    let is_git = app.deploy_type == "git";
    if (req.git_repo_url.is_some()
        || req.git_branch.is_some()
        || req.build_method.is_some()
        || req.git_credential.is_some())
        && !is_git
    {
        return Err(AppError::validation("git settings only apply to git apps"));
    }
    let repo = req.git_repo_url.as_deref().map(str::trim);
    if let Some(repo) = repo {
        validate::git_url(repo)?;
    }
    let branch = req.git_branch.as_deref().map(str::trim);
    if let Some(branch) = branch {
        validate::git_branch(branch)?;
    }
    if let Some(method) = &req.build_method {
        validate_build_method(method)?;
    }
    if let Some(Some(credential)) = &req.git_credential {
        validate_credential(credential)?;
    }
    let is_compose = app.deploy_type == "compose";
    if (req.compose_yaml.is_some() || req.compose_service.is_some()) && !is_compose {
        return Err(AppError::validation(
            "compose settings only apply to compose apps",
        ));
    }
    if is_compose && (req.compose_yaml.is_some() || req.compose_service.is_some()) {
        let yaml = match &req.compose_yaml {
            Some(yaml) => yaml.clone(),
            None => sqlx::query_scalar::<_, Option<String>>(
                "SELECT compose_yaml FROM apps WHERE id = ?",
            )
            .bind(&id)
            .fetch_one(&state.db)
            .await?
            .unwrap_or_default(),
        };
        let services = validate::compose::check_raw(&yaml)?;
        let service = match &req.compose_service {
            Some(s) => s.clone(),
            None => app.compose_service.clone(),
        };
        check_compose_service(service.as_deref(), &services)?;
    }
    if is_compose && req.volumes.is_some() {
        return Err(AppError::validation(
            "compose apps declare volumes in compose_yaml",
        ));
    }
    if let Some(port) = req.container_port {
        validate::port(port)?;
    }
    if let Some(path) = &req.healthcheck_path {
        validate::http_path(path)?;
    }
    if (req.memory_limit_mb.is_some() || req.cpu_limit.is_some()) && is_compose {
        return Err(AppError::validation(
            "compose apps set resource limits per service (PATCH /apps/{id}/services/{service})",
        ));
    }
    if let Some(Some(mb)) = req.memory_limit_mb {
        validate::memory_limit_mb(mb)?;
    }
    if let Some(Some(cpus)) = req.cpu_limit {
        validate::cpu_limit(cpus)?;
    }
    if let Some(Some(secs)) = req.health_timeout_secs {
        validate::health_timeout_secs(secs)?;
    }
    if let Some(env) = &req.env {
        for (key, value) in env {
            validate::env_key(key)?;
            if let Some(input) = value {
                validate::env_value(key, &input.value)?;
            }
        }
    }
    let domains = match &req.domains {
        Some(list) => Some(normalize_domains(&state, &app, list).await?),
        None => None,
    };
    if let Some(volumes) = &req.volumes {
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for v in volumes {
            validate::name(&v.name)?;
            validate::mount_path(&v.mount_path)?;
            if !names.insert(&v.name) || !paths.insert(&v.mount_path) {
                return Err(AppError::validation(
                    "volume names and mount paths must be unique",
                ));
            }
        }
    }

    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE apps SET image_name = COALESCE(?, image_name), \
         container_port = COALESCE(?, container_port), \
         healthcheck_path = COALESCE(?, healthcheck_path), exposed = COALESCE(?, exposed), \
         updated_at = CURRENT_TIMESTAMP \
         WHERE id = ?",
    )
    .bind(image)
    .bind(req.container_port)
    .bind(&req.healthcheck_path)
    .bind(req.exposed)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if let Some(mb) = req.memory_limit_mb {
        sqlx::query("UPDATE apps SET memory_limit_mb = ? WHERE id = ?")
            .bind(mb)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(cpus) = req.cpu_limit {
        sqlx::query("UPDATE apps SET cpu_limit = ? WHERE id = ?")
            .bind(cpus)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(secs) = req.health_timeout_secs {
        sqlx::query("UPDATE apps SET health_timeout_secs = ? WHERE id = ?")
            .bind(secs)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    if is_compose {
        if let Some(yaml) = &req.compose_yaml {
            sqlx::query("UPDATE apps SET compose_yaml = ? WHERE id = ?")
                .bind(yaml)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(service) = &req.compose_service {
            sqlx::query("UPDATE apps SET compose_service = ? WHERE id = ?")
                .bind(service)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
    }
    if is_git {
        sqlx::query(
            "UPDATE apps SET git_repo_url = COALESCE(?, git_repo_url), \
             git_branch = COALESCE(?, git_branch), build_method = COALESCE(?, build_method) \
             WHERE id = ?",
        )
        .bind(repo)
        .bind(branch)
        .bind(&req.build_method)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        if let Some(credential) = &req.git_credential {
            let enc = credential
                .as_deref()
                .map(|c| {
                    state
                        .cipher
                        .encrypt(c, &aad("apps", &id, "git_credential_enc"))
                })
                .transpose()?;
            sqlx::query("UPDATE apps SET git_credential_enc = ? WHERE id = ?")
                .bind(enc)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
    }

    if let Some(env) = &req.env {
        for (key, value) in env {
            let existing: Option<String> =
                sqlx::query_scalar("SELECT id FROM env_vars WHERE app_id = ? AND key = ?")
                    .bind(&id)
                    .bind(key)
                    .fetch_optional(&mut *tx)
                    .await?;
            match (value, existing) {
                (None, Some(row_id)) => {
                    sqlx::query("DELETE FROM env_vars WHERE id = ?")
                        .bind(row_id)
                        .execute(&mut *tx)
                        .await?;
                }
                (None, None) => {}
                (Some(input), existing) => {
                    write_env(
                        &mut tx,
                        &state,
                        &id,
                        existing,
                        key,
                        &input.value,
                        input.is_secret,
                    )
                    .await?;
                }
            }
        }
    }

    if let Some(domains) = &domains {
        sqlx::query("DELETE FROM domains WHERE app_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for (hostname, force_https) in domains {
            sqlx::query(
                "INSERT INTO domains (id, app_id, hostname, force_https) VALUES (?, ?, ?, ?)",
            )
            .bind(new_id())
            .bind(&id)
            .bind(hostname)
            .bind(force_https)
            .execute(&mut *tx)
            .await?;
        }
    }

    if let Some(volumes) = &req.volumes {
        sqlx::query("DELETE FROM volumes WHERE app_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for v in volumes {
            sqlx::query("INSERT INTO volumes (id, app_id, name, mount_path) VALUES (?, ?, ?, ?)")
                .bind(new_id())
                .bind(&id)
                .bind(&v.name)
                .bind(&v.mount_path)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;

    if (domains.is_some() || req.exposed.is_some()) && app.active_deployment_id.is_some() {
        routing::sync_node(&state, &app.node_id).await?;
    }
    let redeploy_required = app.active_deployment_id.is_some()
        && (req.image_name.is_some()
            || req.container_port.is_some()
            || req.healthcheck_path.is_some()
            || req.memory_limit_mb.is_some()
            || req.cpu_limit.is_some()
            || req.env.is_some()
            || req.volumes.is_some()
            || req.git_repo_url.is_some()
            || req.git_branch.is_some()
            || req.build_method.is_some()
            || req.compose_yaml.is_some()
            || req.compose_service.is_some());
    Ok(Json(UpdateResponse {
        detail: detail(&state, &id).await?,
        redeploy_required,
    }))
}

async fn normalize_domains(
    state: &AppState,
    app: &App,
    list: &[DomainInput],
) -> AppResult<Vec<(String, bool)>> {
    let panel = routing::panel_domain(&state.db).await?;
    let names = PublicNames::load(&state.db).await?;
    let others: Vec<String> = sqlx::query_scalar("SELECT name FROM apps WHERE id != ?")
        .bind(&app.id)
        .fetch_all(&state.db)
        .await?;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(list.len());
    for d in list {
        let hostname = d.hostname.trim().to_ascii_lowercase();
        validate::hostname(&hostname)?;
        if panel.as_deref() == Some(hostname.as_str()) {
            return Err(AppError::Conflict(format!(
                "{hostname} is the panel domain"
            )));
        }
        if let Some(owner) = others
            .iter()
            .find(|n| names.candidate(n).as_deref() == Some(hostname.as_str()))
        {
            return Err(AppError::Conflict(format!(
                "{hostname} is the default domain of app {owner}"
            )));
        }
        if !seen.insert(hostname.clone()) {
            return Err(AppError::validation(format!("duplicate domain {hostname}")));
        }
        out.push((hostname, d.force_https));
    }
    Ok(out)
}

#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    remove_volumes: bool,
}

async fn remove(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> AppResult<axum::http::StatusCode> {
    // Wait for any running deployment of this app to finish first.
    let lock = state.deploy_locks.get(&id);
    let _guard = lock.lock().await;
    let app = fetch_app(&state, &id).await?;
    let executor = state.registry.get(&app.node_id)?;

    let volumes: Vec<String> = sqlx::query_scalar("SELECT name FROM volumes WHERE app_id = ?")
        .bind(&id)
        .fetch_all(&state.db)
        .await?;
    let logs: Vec<Option<String>> =
        sqlx::query_scalar("SELECT log_path FROM deployments WHERE app_id = ?")
            .bind(&id)
            .fetch_all(&state.db)
            .await?;

    if app.deploy_type == "compose" {
        // `down` also removes the project's networks (and volumes when requested).
        executor
            .remove_compose(&compose_project(&app.name), query.remove_volumes)
            .await?;
    } else if let Some(container) = &app.active_container_id {
        executor.remove_container(container).await?;
    }
    sqlx::query("DELETE FROM apps WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    routing::sync_node(&state, &app.node_id).await?;

    if query.remove_volumes && app.deploy_type != "compose" {
        let names: Vec<String> = volumes.iter().map(|v| volume_name(&app.name, v)).collect();
        executor.remove_volumes(&names).await?;
    }
    for path in logs.into_iter().flatten() {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct DeployResponse {
    deployment_id: String,
}

async fn deploy(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<DeployResponse>)> {
    let app = fetch_app(&state, &id).await?;
    let deployment_id = pipeline::enqueue(&state, &app, pipeline::Trigger::Build).await?;
    Ok((StatusCode::ACCEPTED, Json(DeployResponse { deployment_id })))
}

async fn control(
    _: AuthUser,
    State(state): State<AppState>,
    Path((id, action)): Path<(String, String)>,
) -> AppResult<Json<App>> {
    let (action, status) = match action.as_str() {
        "start" => (ContainerAction::Start, "running"),
        "stop" => (ContainerAction::Stop, "stopped"),
        "restart" => (ContainerAction::Restart, "running"),
        _ => return Err(AppError::NotFound),
    };
    // Self-healing holds this while it checks and acts, so it can never undo this action.
    let lock = state.container_locks.get(&id);
    let _guard = lock.lock().await;
    let app = fetch_app(&state, &id).await?;
    if app.active_deployment_id.is_none() {
        return Err(AppError::Conflict("app has no deployed container".into()));
    }
    // A finishing deployment sets the app running again and would silently undo this.
    // New deployments queued after this check are a later request and win, as intended.
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployments WHERE app_id = ? AND status IN ('queued', 'building')",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    if pending > 0 {
        return Err(AppError::Conflict(
            "a deployment is in progress; wait for it to finish".into(),
        ));
    }
    let executor = state.registry.get(&app.node_id)?;
    // Compose apps: every container of the stack. Others: the active container.
    let containers: Vec<String> = if app.deploy_type == "compose" {
        executor
            .list_managed_containers()
            .await?
            .into_iter()
            .filter(|c| c.app_id == app.id)
            .map(|c| c.id)
            .collect()
    } else {
        app.active_container_id.clone().into_iter().collect()
    };
    for container in &containers {
        executor.set_container_state(container, action).await?;
    }
    sqlx::query("UPDATE apps SET status = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(status)
        .bind(&id)
        .execute(&state.db)
        .await?;
    Ok(Json(fetch_app(&state, &id).await?))
}

#[derive(Serialize)]
struct WebhookInfo {
    /// Append to the panel's public URL, e.g. `https://panel.example.com/webhooks/git/<id>`.
    path: String,
    secret: String,
    branch: Option<String>,
}

/// Secret for configuring the git provider's push webhook (GitHub/Gitea: HMAC-SHA256
/// "secret"; GitLab: "secret token").
async fn webhook_info(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<WebhookInfo>> {
    let app = fetch_app(&state, &id).await?;
    let enc: Option<String> =
        sqlx::query_scalar("SELECT webhook_secret_enc FROM apps WHERE id = ?")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    let enc = enc.ok_or_else(|| AppError::validation("only git apps have a deploy webhook"))?;
    let secret = state
        .cipher
        .decrypt(&enc, &aad("apps", &id, "webhook_secret_enc"))?;
    Ok(Json(WebhookInfo {
        path: format!("/webhooks/git/{id}"),
        secret,
        branch: app.git_branch,
    }))
}

/// WebSocket: live stdout/stderr of the app's active container.
async fn container_logs(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::WebSocketUpgrade,
) -> AppResult<axum::response::Response> {
    super::ws::check_origin(&headers)?;
    let app = fetch_app(&state, &id).await?;
    let container = app
        .active_container_id
        .ok_or_else(|| AppError::Conflict("app has no deployed container".into()))?;
    let lines = state
        .registry
        .get(&app.node_id)?
        .stream_logs(&container)
        .await?;
    Ok(ws.on_upgrade(move |socket| super::ws::forward_lines(socket, lines)))
}

#[derive(Serialize)]
struct ComposeServiceView {
    name: String,
    container_name: String,
    image: Option<String>,
    environment: Vec<ComposeEnvironmentView>,
    volumes: Vec<String>,
    ports: Vec<String>,
    depends_on: Vec<String>,
    /// From `mem_limit` (or `deploy.resources.limits.memory`), in MiB.
    memory_limit_mb: Option<i64>,
    /// From `cpus` (or `deploy.resources.limits.cpus`).
    cpu_limit: Option<f64>,
    present: bool,
    running: bool,
    restarting: bool,
    restart_count: i64,
    health: Option<String>,
    exposed: bool,
}

#[derive(Serialize, Clone)]
struct ComposeEnvironmentView {
    key: String,
    value: Option<String>,
}

struct ComposeServiceConfig {
    image: Option<String>,
    environment: Vec<ComposeEnvironmentView>,
    volumes: Vec<String>,
    ports: Vec<String>,
    depends_on: Vec<String>,
    memory_limit_mb: Option<i64>,
    cpu_limit: Option<f64>,
}

struct ComposeDocument {
    document: Value,
    services: Vec<String>,
    yaml: String,
}

async fn compose_document(state: &AppState, app: &App) -> AppResult<ComposeDocument> {
    if app.deploy_type != "compose" {
        return Err(AppError::validation("services only apply to compose apps"));
    }
    let yaml: Option<String> = sqlx::query_scalar("SELECT compose_yaml FROM apps WHERE id = ?")
        .bind(&app.id)
        .fetch_one(&state.db)
        .await?;
    let yaml = yaml.ok_or_else(|| AppError::validation("compose app has no compose file"))?;
    let services = validate::compose::check_raw(&yaml)?;
    Ok(ComposeDocument {
        document: validate::compose::parse(&yaml)?,
        services,
        yaml,
    })
}

fn list_values(service: &Value, key: &str) -> Vec<String> {
    let Some(value) = service.get(key) else {
        return vec![];
    };
    match value {
        Value::Array(values) => values.iter().filter_map(display_value).collect(),
        Value::Object(values) => values.keys().cloned().collect(),
        _ => vec![],
    }
}

fn display_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Object(value) => {
            let source = value.get("source").and_then(Value::as_str);
            let target = value.get("target").and_then(Value::as_str);
            match (source, target) {
                (Some(source), Some(target)) => Some(format!(
                    "{source}:{target}{}",
                    if value.get("read_only").and_then(Value::as_bool) == Some(true) {
                        ":ro"
                    } else {
                        ""
                    }
                )),
                _ => serde_json::to_string(value).ok(),
            }
        }
        _ => None,
    }
}

fn environment_entries(service: &Value) -> Vec<ComposeEnvironmentView> {
    let Some(value) = service.get("environment") else {
        return vec![];
    };
    let mut entries = match value {
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| ComposeEnvironmentView {
                key: key.clone(),
                value: value.as_str().map(String::from),
            })
            .collect(),
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| {
                let (key, value) = value
                    .split_once('=')
                    .map_or((value, None), |(key, value)| (key, Some(value.to_string())));
                ComposeEnvironmentView {
                    key: key.to_string(),
                    value,
                }
            })
            .collect(),
        _ => vec![],
    };
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

fn compose_service_config(document: &Value, service: &str) -> ComposeServiceConfig {
    let service = document
        .get("services")
        .and_then(|services| services.get(service))
        .unwrap_or(&Value::Null);
    ComposeServiceConfig {
        image: service
            .get("image")
            .and_then(Value::as_str)
            .map(String::from),
        environment: environment_entries(service),
        volumes: list_values(service, "volumes"),
        ports: list_values(service, "ports"),
        depends_on: list_values(service, "depends_on"),
        memory_limit_mb: limit(service, "mem_limit", "memory").and_then(|v| memory_mb(&v)),
        cpu_limit: limit(service, "cpus", "cpus").and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }),
    }
}

/// A service's limit, from the short key or from `deploy.resources.limits.<long>`.
fn limit(service: &Value, short: &str, long: &str) -> Option<Value> {
    service
        .get(short)
        .or_else(|| service.pointer(&format!("/deploy/resources/limits/{long}")))
        .cloned()
}

/// Compose memory values: bytes as a number, or a string such as `512m` or `1.5g`.
fn memory_mb(value: &Value) -> Option<i64> {
    let bytes = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => {
            let s = s.trim().to_ascii_lowercase();
            let s = s.strip_suffix('b').unwrap_or(&s);
            let (number, unit) = match s.char_indices().last()? {
                (i, c) if c.is_ascii_alphabetic() => (&s[..i], c),
                _ => (s, ' '),
            };
            let scale = match unit {
                ' ' => 1.0,
                'k' => 1024.0,
                'm' => 1024.0 * 1024.0,
                'g' => 1024.0 * 1024.0 * 1024.0,
                't' => 1024.0 * 1024.0 * 1024.0 * 1024.0,
                _ => return None,
            };
            number.trim().parse::<f64>().ok()? * scale
        }
        _ => return None,
    };
    Some((bytes / (1024.0 * 1024.0)).round() as i64)
}

#[derive(Deserialize)]
struct ServiceEnvironmentInput {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct ServiceUpdate {
    #[serde(default)]
    environment: Option<Vec<ServiceEnvironmentInput>>,
    #[serde(default)]
    ports: Option<Vec<String>>,
    #[serde(default)]
    volumes: Option<Vec<String>>,
    /// `null` removes the limit.
    #[serde(default, deserialize_with = "present")]
    memory_limit_mb: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    cpu_limit: Option<Option<f64>>,
}

#[derive(Serialize)]
struct ServiceUpdateResponse {
    #[serde(flatten)]
    service: ComposeServiceView,
    redeploy_required: bool,
}

fn compose_with_service_settings(
    yaml: &str,
    service: &str,
    update: &ServiceUpdate,
) -> AppResult<String> {
    let environment = update.environment.as_deref();
    let (ports, volumes) = (update.ports.as_deref(), update.volumes.as_deref());
    let mut document = validate::compose::parse(yaml)?;
    let services = document
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AppError::validation("compose: services must be a mapping"))?;
    let service_doc = services
        .get_mut(service)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            AppError::validation(format!("compose service `{service}` is not a mapping"))
        })?;

    if let Some(environment) = environment {
        let mut seen = HashSet::new();
        let mut values = serde_json::Map::new();
        for entry in environment {
            let key = entry.key.trim();
            validate::env_key(key)?;
            validate::env_value(key, &entry.value)?;
            if !seen.insert(key.to_string()) {
                return Err(AppError::validation(format!(
                    "duplicate environment key {key}"
                )));
            }
            values.insert(key.to_string(), Value::String(entry.value.clone()));
        }
        if values.is_empty() {
            service_doc.remove("environment");
        } else {
            service_doc.insert("environment".into(), Value::Object(values));
        }
    }

    for (key, values) in [("ports", ports), ("volumes", volumes)] {
        let Some(values) = values else {
            continue;
        };
        let mut seen = HashSet::new();
        let mut entries = Vec::with_capacity(values.len());
        for value in values {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if value.contains('\0') {
                return Err(AppError::validation(format!(
                    "compose service {key} entries cannot contain null bytes"
                )));
            }
            if !seen.insert(value) {
                return Err(AppError::validation(format!(
                    "duplicate {key} entry {value}"
                )));
            }
            entries.push(Value::String(value.to_string()));
        }
        if entries.is_empty() {
            service_doc.remove(key);
        } else {
            service_doc.insert(key.into(), Value::Array(entries));
        }
    }

    // Limits are written as `mem_limit`/`cpus`; a `deploy.resources.limits` entry for the
    // same resource is dropped so the two cannot disagree.
    if let Some(memory) = update.memory_limit_mb {
        drop_deploy_limit(service_doc, "memory");
        match memory {
            Some(mb) => {
                validate::memory_limit_mb(mb)?;
                service_doc.insert("mem_limit".into(), Value::String(format!("{mb}m")));
            }
            None => {
                service_doc.remove("mem_limit");
            }
        }
    }
    if let Some(cpus) = update.cpu_limit {
        drop_deploy_limit(service_doc, "cpus");
        match cpus {
            Some(cores) => {
                validate::cpu_limit(cores)?;
                service_doc.insert("cpus".into(), serde_json::json!(cores));
            }
            None => {
                service_doc.remove("cpus");
            }
        }
    }

    let updated = serde_yaml_ng::to_string(&document)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    validate::compose::check_raw(&updated)?;
    Ok(updated)
}

fn drop_deploy_limit(service: &mut serde_json::Map<String, Value>, key: &str) {
    if let Some(limits) = service
        .get_mut("deploy")
        .and_then(|d| d.get_mut("resources"))
        .and_then(|r| r.get_mut("limits"))
        .and_then(Value::as_object_mut)
    {
        limits.remove(key);
    }
}

async fn managed_compose_containers(
    state: &AppState,
    app: &App,
) -> AppResult<HashMap<String, ContainerHealth>> {
    Ok(state
        .registry
        .get(&app.node_id)?
        .list_managed_containers()
        .await?
        .into_iter()
        .filter(|container| container.app_id == app.id)
        .map(|container| (container.name.clone(), container))
        .collect())
}

fn compose_service_view(
    app: &App,
    service: String,
    config: ComposeServiceConfig,
    container: Option<&ContainerHealth>,
) -> ComposeServiceView {
    let container_name = compose_container_name(&app.name, &service);
    ComposeServiceView {
        exposed: app.compose_service.as_deref() == Some(service.as_str()),
        name: service,
        container_name,
        image: config.image,
        environment: config.environment,
        volumes: config.volumes,
        ports: config.ports,
        depends_on: config.depends_on,
        memory_limit_mb: config.memory_limit_mb,
        cpu_limit: config.cpu_limit,
        present: container.is_some(),
        running: container.is_some_and(|c| c.running),
        restarting: container.is_some_and(|c| c.restarting),
        restart_count: container.map_or(0, |c| c.restart_count),
        health: container.and_then(|c| c.health.clone()),
    }
}

/// Runtime state for every service declared in the app's Compose file.
async fn list_services(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<ComposeServiceView>>> {
    let app = fetch_app(&state, &id).await?;
    let compose = compose_document(&state, &app).await?;
    let containers = managed_compose_containers(&state, &app).await?;
    Ok(Json(
        compose
            .services
            .into_iter()
            .map(|service| {
                let name = compose_container_name(&app.name, &service);
                let config = compose_service_config(&compose.document, &service);
                compose_service_view(&app, service, config, containers.get(&name))
            })
            .collect(),
    ))
}

async fn update_service(
    _: AuthUser,
    State(state): State<AppState>,
    Path((id, service)): Path<(String, String)>,
    Json(req): Json<ServiceUpdate>,
) -> AppResult<Json<ServiceUpdateResponse>> {
    let lock = state.deploy_locks.get(&id);
    let _guard = lock.lock().await;
    let app = fetch_app(&state, &id).await?;
    let compose = compose_document(&state, &app).await?;
    check_compose_service(Some(&service), &compose.services)?;
    if req.environment.is_none()
        && req.ports.is_none()
        && req.volumes.is_none()
        && req.memory_limit_mb.is_none()
        && req.cpu_limit.is_none()
    {
        return Err(AppError::validation(
            "at least one service setting is required",
        ));
    }
    let updated_yaml = compose_with_service_settings(&compose.yaml, &service, &req)?;
    sqlx::query("UPDATE apps SET compose_yaml = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(&updated_yaml)
        .bind(&id)
        .execute(&state.db)
        .await?;

    let document = validate::compose::parse(&updated_yaml)?;
    let containers = managed_compose_containers(&state, &app).await?;
    let container_name = compose_container_name(&app.name, &service);
    let config = compose_service_config(&document, &service);
    Ok(Json(ServiceUpdateResponse {
        service: compose_service_view(&app, service, config, containers.get(&container_name)),
        redeploy_required: app.active_deployment_id.is_some(),
    }))
}

async fn service_control(
    _: AuthUser,
    State(state): State<AppState>,
    Path((id, service, action)): Path<(String, String, String)>,
) -> AppResult<Json<ComposeServiceView>> {
    let action = match action.as_str() {
        "start" => ContainerAction::Start,
        "restart" => ContainerAction::Restart,
        _ => return Err(AppError::NotFound),
    };
    let lock = state.container_locks.get(&id);
    let _guard = lock.lock().await;
    let app = fetch_app(&state, &id).await?;
    let compose = compose_document(&state, &app).await?;
    check_compose_service(Some(&service), &compose.services)?;
    if app.active_deployment_id.is_none() {
        return Err(AppError::Conflict("app has no deployed stack".into()));
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployments WHERE app_id = ? AND status IN ('queued', 'building')",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    if pending > 0 {
        return Err(AppError::Conflict(
            "a deployment is in progress; wait for it to finish".into(),
        ));
    }
    let expected_name = compose_container_name(&app.name, &service);
    let containers = managed_compose_containers(&state, &app).await?;
    let container = containers.get(&expected_name).ok_or_else(|| {
        AppError::Conflict(format!(
            "compose service `{service}` has no deployed container"
        ))
    })?;
    state
        .registry
        .get(&app.node_id)?
        .set_container_state(&container.id, action)
        .await?;
    let refreshed = managed_compose_containers(&state, &app).await?;
    let config = compose_service_config(&compose.document, &service);
    Ok(Json(compose_service_view(
        &app,
        service,
        config,
        refreshed.get(&expected_name),
    )))
}

/// WebSocket: live stdout/stderr for one service in a Compose stack.
async fn service_logs(
    _: AuthUser,
    State(state): State<AppState>,
    Path((id, service)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::WebSocketUpgrade,
) -> AppResult<axum::response::Response> {
    super::ws::check_origin(&headers)?;
    let app = fetch_app(&state, &id).await?;
    let compose = compose_document(&state, &app).await?;
    check_compose_service(Some(&service), &compose.services)?;
    let expected_name = compose_container_name(&app.name, &service);
    let containers = managed_compose_containers(&state, &app).await?;
    let container = containers.get(&expected_name).ok_or_else(|| {
        AppError::Conflict(format!(
            "compose service `{service}` has no deployed container"
        ))
    })?;
    let lines = state
        .registry
        .get(&app.node_id)?
        .stream_logs(&container.id)
        .await?;
    Ok(ws.on_upgrade(move |socket| super::ws::forward_lines(socket, lines)))
}

#[derive(Serialize)]
struct ComposeFile {
    compose_yaml: String,
}

/// The stored compose file of a compose app (as written, before interpolation).
async fn compose_file(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<ComposeFile>> {
    let yaml: Option<String> = sqlx::query_scalar("SELECT compose_yaml FROM apps WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let compose_yaml = yaml.ok_or_else(|| AppError::validation("not a compose app"))?;
    Ok(Json(ComposeFile { compose_yaml }))
}

/// Current CPU and memory usage of the app's running containers (every service of a
/// compose app). Empty when nothing is running.
async fn stats(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<crate::executor::ContainerStats>>> {
    let app = fetch_app(&state, &id).await?;
    let ids: Vec<String> = if app.deploy_type == "compose" {
        managed_compose_containers(&state, &app)
            .await?
            .into_values()
            .filter(|c| c.running)
            .map(|c| c.id)
            .collect()
    } else {
        app.active_container_id.clone().into_iter().collect()
    };
    if ids.is_empty() {
        return Ok(Json(vec![]));
    }
    let mut stats = state
        .registry
        .get(&app.node_id)?
        .container_stats(&ids)
        .await?;
    stats.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(stats))
}

/// TLS certificate state of each of the app's domains, as served by its node's proxy.
async fn certificates(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<crate::executor::CertStatus>>> {
    let app = fetch_app(&state, &id).await?;
    let custom: Vec<String> =
        sqlx::query_scalar("SELECT hostname FROM domains WHERE app_id = ? ORDER BY hostname")
            .bind(&id)
            .fetch_all(&state.db)
            .await?;
    let hostnames = PublicNames::load(&state.db)
        .await?
        .hostnames(&app.name, app.exposed, &custom);
    if hostnames.is_empty() {
        return Ok(Json(vec![]));
    }
    Ok(Json(
        state
            .registry
            .get(&app.node_id)?
            .probe_certificates(&hostnames)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_compose_memory_values() {
        for (value, mb) in [
            (json!("512m"), Some(512)),
            (json!("1g"), Some(1024)),
            (json!("1.5G"), Some(1536)),
            (json!("256mb"), Some(256)),
            (json!(1_073_741_824), Some(1024)),
            (json!("lots"), None),
        ] {
            assert_eq!(memory_mb(&value), mb, "{value}");
        }
        let service = json!({"deploy": {"resources": {"limits": {"memory": "2g", "cpus": "0.5"}}}});
        assert_eq!(limit(&service, "mem_limit", "memory"), Some(json!("2g")));
        assert_eq!(limit(&service, "cpus", "cpus"), Some(json!("0.5")));
    }
}
