use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::AppState;
use crate::auth::AuthUser;
use crate::db::models::Deployment;
use crate::deploy::pipeline::{self, Trigger};
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{id}", get(show))
        .route("/{id}/log", get(log))
        .route("/{id}/logs", get(log_stream))
        .route("/{id}/rollback", post(rollback))
}

async fn fetch(state: &AppState, id: &str) -> AppResult<Deployment> {
    sqlx::query_as::<_, Deployment>(concat!(
        "SELECT ",
        crate::deployment_columns!(),
        " FROM deployments WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn show(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Deployment>> {
    Ok(Json(fetch(&state, &id).await?))
}

async fn log_path(state: &AppState, id: &str) -> AppResult<Option<String>> {
    sqlx::query_scalar("SELECT log_path FROM deployments WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Persisted build/deploy log as plain text.
async fn log(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let text = match log_path(&state, &id).await? {
        Some(path) => match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(anyhow::Error::from(err).into()),
        },
        None => String::new(),
    };
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text))
}

/// WebSocket: replays the log file from the start, then tails it until the deployment
/// finishes. The file is the source of truth, so slow clients never lose lines.
async fn log_stream(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    super::ws::check_origin(&headers)?;
    let path = log_path(&state, &id).await?;
    Ok(ws.on_upgrade(move |socket| tail(socket, state, id, path)))
}

async fn tail(mut socket: WebSocket, state: AppState, id: String, path: Option<String>) {
    let mut offset = 0u64;
    let mut pending = String::new();
    loop {
        // Check status before reading so the final lines are always sent.
        let finished = match fetch(&state, &id).await {
            Ok(d) => !matches!(d.status.as_str(), "queued" | "building"),
            Err(_) => true,
        };
        if let Some(path) = &path
            && let Ok(mut file) = tokio::fs::File::open(path).await
            && file.seek(std::io::SeekFrom::Start(offset)).await.is_ok()
        {
            let mut buf = Vec::new();
            if let Ok(n) = file.read_to_end(&mut buf).await {
                offset += n as u64;
                pending.push_str(&String::from_utf8_lossy(&buf));
                while let Some(pos) = pending.find('\n') {
                    let line: String = pending.drain(..=pos).collect();
                    let text = line.trim_end_matches('\n').to_string();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
        if finished {
            break;
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
            incoming = socket.recv() => if matches!(incoming, Some(Ok(Message::Close(_))) | None | Some(Err(_))) {
                return;
            },
        }
    }
    if !pending.is_empty() {
        let _ = socket.send(Message::Text(pending.into())).await;
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[derive(Serialize)]
struct RollbackResponse {
    deployment_id: String,
}

/// Redeploy the image of a previous healthy deployment (no rebuild).
async fn rollback(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<RollbackResponse>)> {
    let target = fetch(&state, &id).await?;
    if !matches!(target.status.as_str(), "healthy" | "rolled_back") {
        return Err(AppError::Conflict(
            "only previously healthy deployments can be rolled back to".into(),
        ));
    }
    let image_ref = target
        .image_ref
        .clone()
        .ok_or_else(|| AppError::Conflict("deployment has no image".into()))?;
    let app = sqlx::query_as::<_, crate::db::models::App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(&target.app_id)
    .fetch_one(&state.db)
    .await?;
    if app.active_deployment_id.as_deref() == Some(target.id.as_str()) {
        return Err(AppError::Conflict("deployment is already live".into()));
    }
    let deployment_id = pipeline::enqueue(
        &state,
        &app,
        Trigger::Rollback {
            image_ref,
            commit_sha: target.commit_sha,
        },
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(RollbackResponse { deployment_id }),
    ))
}
