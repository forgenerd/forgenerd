use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use super::AppState;
use crate::auth::AuthUser;
use crate::db::models::Node;
use crate::error::AppResult;

pub fn routes() -> Router<AppState> {
    Router::new().route("/", get(list))
}

async fn list(_auth: AuthUser, State(state): State<AppState>) -> AppResult<Json<Vec<Node>>> {
    let nodes = sqlx::query_as::<_, Node>("SELECT * FROM nodes ORDER BY created_at")
        .fetch_all(&state.db)
        .await?;
    Ok(Json(nodes))
}
