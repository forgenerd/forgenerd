use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::AppState;
use super::apps::{AppDetail, detail, write_env};
use crate::auth::AuthUser;
use crate::blueprints;
use crate::db::new_id;
use crate::deploy::pipeline::{self, Trigger};
use crate::deploy::routing::PublicNames;
use crate::error::{AppError, AppResult};
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/refresh", post(refresh))
        .route("/{id}/deploy", post(deploy))
}

#[derive(Serialize)]
struct BlueprintSummary {
    id: String,
    name: String,
    category: String,
    description: String,
    /// Served publicly on the app's domain.
    expose: bool,
    port: u16,
    image: String,
    image_tag: String,
    /// `data:image/png;base64,...`, when the catalog has one.
    logo: Option<String>,
    parameters: Vec<BlueprintParameter>,
}

#[derive(Serialize)]
struct BlueprintList {
    blueprints: Vec<BlueprintSummary>,
    /// When the catalog was downloaded; null when none has been yet.
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the last refresh failed; the previous catalog is still offered.
    error: Option<String>,
}

#[derive(Serialize)]
struct BlueprintParameter {
    key: String,
    default_value: Option<String>,
    description: String,
    secret: bool,
    generate: bool,
    /// No default: the deploy form must supply a value.
    required: bool,
    /// Defaults to the app's public URL, filled in at deploy time.
    public_url: bool,
}

async fn list(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<BlueprintList>> {
    Ok(Json(summarize(&state.catalog.snapshot())))
}

/// Download the catalog now (the panel also refreshes it every few hours).
async fn refresh(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<BlueprintList>> {
    state
        .catalog
        .refresh(&blueprints::catalog_url(), &state.data_dir)
        .await?;
    Ok(Json(summarize(&state.catalog.snapshot())))
}

fn summarize(snapshot: &blueprints::Snapshot) -> BlueprintList {
    BlueprintList {
        updated_at: snapshot.updated_at,
        error: snapshot.error.clone(),
        blueprints: snapshot
            .blueprints
            .iter()
            .cloned()
            .map(|t| BlueprintSummary {
                id: t.id,
                name: t.name,
                category: t.category,
                description: t.description,
                expose: t.expose,
                port: t.port,
                image: t.image,
                image_tag: t.image_tag,
                logo: t.logo,
                parameters: t
                    .env
                    .into_iter()
                    .map(|(key, spec)| {
                        let required = spec.required();
                        let public_url = spec.value.as_deref().is_some_and(|v| v.contains("{url}"));
                        BlueprintParameter {
                            key,
                            default_value: if spec.secret || spec.generate || public_url {
                                None
                            } else {
                                spec.value
                            },
                            description: spec.description,
                            secret: spec.secret,
                            generate: spec.generate,
                            required,
                            public_url,
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[derive(Deserialize)]
struct DeployBlueprint {
    project_id: String,
    name: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    image_tag: Option<String>,
}

#[derive(Serialize)]
struct BlueprintDeployed {
    app: AppDetail,
    deployment_id: String,
    /// Hostname other apps use to reach this service on `forgenerd-internal`.
    internal_host: String,
    /// Includes generated credentials. Shown only in this response: secret env vars are
    /// never returned again.
    connection_string: Option<String>,
    /// Where an exposed app is served (absent until a root domain is set).
    url: Option<String>,
    /// Generated passwords and keys, shown once so admin logins can be used.
    generated: Vec<GeneratedValue>,
}

#[derive(Serialize)]
struct GeneratedValue {
    key: String,
    value: String,
}

/// Create a compose app from a blueprint (generating its secrets) and deploy it.
async fn deploy(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DeployBlueprint>,
) -> AppResult<(StatusCode, Json<BlueprintDeployed>)> {
    let blueprint = state.catalog.find(&id)?;
    let name = req.name.trim().to_ascii_lowercase();
    validate::app_name(&name)?;
    let image_tag = req
        .image_tag
        .as_deref()
        .unwrap_or(&blueprint.image_tag)
        .trim()
        .to_string();
    validate::image_tag(&image_tag)?;
    validate::image_ref(&format!("{}:{image_tag}", blueprint.image))?;
    let compose = blueprint.compose.replace("${IMAGE_TAG}", &image_tag);
    for (key, value) in &req.env {
        let Some(spec) = blueprint.env.get(key) else {
            return Err(AppError::validation(format!(
                "unknown blueprint parameter: {key}"
            )));
        };
        if value.trim().is_empty() && spec.required() {
            return Err(AppError::validation(format!("{key} cannot be empty")));
        }
    }
    for (key, spec) in &blueprint.env {
        if spec.required() && req.env.get(key).is_none_or(|v| v.trim().is_empty()) {
            return Err(AppError::validation(format!("{key} is required")));
        }
    }
    let services = validate::compose::check_raw(&compose)?;
    super::apps::check_compose_service(Some(&blueprint.service), &services)?;

    let project: Option<String> = sqlx::query_scalar("SELECT id FROM projects WHERE id = ?")
        .bind(&req.project_id)
        .fetch_optional(&state.db)
        .await?;
    if project.is_none() {
        return Err(AppError::validation("project_id does not exist"));
    }

    // Databases stay internal; web apps are served on `<name>.<root_domain>`. Apps that
    // need their own URL get it in their settings (editable later, e.g. after adding a
    // custom domain).
    let url = if blueprint.expose {
        PublicNames::load(&state.db)
            .await?
            .default_for(&name)
            .map(|host| format!("https://{host}"))
    } else {
        None
    };
    let app_id = new_id();
    let mut env = blueprint.resolve_env(url.as_deref().unwrap_or("http://localhost"));
    for (key, value, _) in &mut env {
        if let Some(override_value) = req.env.get(key)
            && !override_value.trim().is_empty()
        {
            *value = override_value.clone();
        }
    }
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO apps (id, project_id, name, deploy_type, compose_yaml, compose_service, container_port, exposed) \
         VALUES (?, ?, ?, 'compose', ?, ?, ?, ?)",
    )
    .bind(&app_id)
    .bind(&req.project_id)
    .bind(&name)
        .bind(&compose)
    .bind(&blueprint.service)
    .bind(i64::from(blueprint.port))
    .bind(blueprint.expose)
    .execute(&mut *tx)
    .await?;
    for (key, value, secret) in &env {
        write_env(&mut tx, &state, &app_id, None, key, value, *secret).await?;
    }
    tx.commit().await?;

    let app = detail(&state, &app_id).await?;
    let deployment_id = pipeline::enqueue(&state, &app.app, Trigger::Build).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(BlueprintDeployed {
            connection_string: blueprint.connection_string(&name, &env),
            url,
            generated: env
                .iter()
                .filter(|(key, ..)| blueprint.env.get(key).is_some_and(|spec| spec.generate))
                .map(|(key, value, _)| GeneratedValue {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            internal_host: name,
            app,
            deployment_id,
        }),
    ))
}
