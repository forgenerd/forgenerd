use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;

use super::AppState;
use crate::auth::AuthUser;
use crate::db::models::Project;
use crate::db::new_id;
use crate::error::{AppError, AppResult};
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", delete(remove))
}

async fn list(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<Vec<Project>>> {
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    Ok(Json(projects))
}

#[derive(Deserialize)]
struct CreateProject {
    name: String,
}

async fn create(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateProject>,
) -> AppResult<(StatusCode, Json<Project>)> {
    let name = req.name.trim().to_ascii_lowercase();
    validate::name(&name)?;
    let project =
        sqlx::query_as::<_, Project>("INSERT INTO projects (id, name) VALUES (?, ?) RETURNING *")
            .bind(new_id())
            .bind(name)
            .fetch_one(&state.db)
            .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn remove(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    // Deleting would cascade to app rows while their containers keep running.
    let apps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps WHERE project_id = ?")
        .bind(&id)
        .fetch_one(&state.db)
        .await?;
    if apps > 0 {
        return Err(AppError::Conflict("delete the project's apps first".into()));
    }
    let result = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
