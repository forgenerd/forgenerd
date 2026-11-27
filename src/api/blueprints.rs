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
use crate::error::{AppError, AppResult};
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{id}/deploy", post(deploy))
}

#[derive(Serialize)]
struct BlueprintSummary {
    id: String,
    name: String,
    description: String,
    port: u16,
    image: String,
    image_tag: String,
    parameters: Vec<BlueprintParameter>,
}

#[derive(Serialize)]
struct BlueprintParameter {
    key: String,
    default_value: Option<String>,
    description: String,
    secret: bool,
    generate: bool,
}

async fn list(_: AuthUser) -> AppResult<Json<Vec<BlueprintSummary>>> {
    Ok(Json(
        blueprints::all()?
            .into_iter()
            .map(|t| BlueprintSummary {
                id: t.id,
                name: t.name,
                description: t.description,
                port: t.port,
                image: t.image,
                image_tag: t.image_tag,
                parameters: t
                    .env
                    .into_iter()
                    .map(|(key, spec)| BlueprintParameter {
                        key,
                        default_value: if spec.secret || spec.generate {
                            None
                        } else {
                            spec.value
                        },
                        description: spec.description,
                        secret: spec.secret,
                        generate: spec.generate,
                    })
                    .collect(),
            })
            .collect(),
    ))
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
}

/// Create a compose app from a blueprint (generating its secrets) and deploy it.
async fn deploy(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DeployBlueprint>,
) -> AppResult<(StatusCode, Json<BlueprintDeployed>)> {
    let blueprint = blueprints::find(&id)?;
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
        if value.trim().is_empty() && !spec.generate {
            return Err(AppError::validation(format!("{key} cannot be empty")));
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

    // Databases stay internal (exposed = 0); other apps reach them by name.
    let app_id = new_id();
    let mut env = blueprint.resolve_env();
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
         VALUES (?, ?, ?, 'compose', ?, ?, ?, 0)",
    )
    .bind(&app_id)
    .bind(&req.project_id)
    .bind(&name)
        .bind(&compose)
    .bind(&blueprint.service)
    .bind(i64::from(blueprint.port))
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
            internal_host: name,
            app,
            deployment_id,
        }),
    ))
}
