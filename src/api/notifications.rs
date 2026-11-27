use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::auth::AuthUser;
use crate::auth::crypto::aad;
use crate::db::new_id;
use crate::error::{AppError, AppResult};
use crate::notify::{self, Event, KINDS};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", delete(remove))
        .route("/{id}/test", post(test))
}

#[derive(Serialize)]
struct Target {
    id: String,
    kind: String,
    /// Only the host is shown: webhook URLs embed their secret in the path.
    host: String,
    enabled: bool,
}

async fn load(state: &AppState) -> AppResult<Vec<Target>> {
    let rows: Vec<(String, String, String, bool)> =
        sqlx::query_as("SELECT id, kind, url_enc, enabled FROM notification_targets ORDER BY id")
            .fetch_all(&state.db)
            .await?;
    rows.into_iter()
        .map(|(id, kind, url_enc, enabled)| {
            let url = state
                .cipher
                .decrypt(&url_enc, &aad("notification_targets", &id, "url_enc"))?;
            let host = url
                .split_once("://")
                .map(|(_, rest)| rest.split('/').next().unwrap_or_default().to_string())
                .unwrap_or_default();
            Ok(Target {
                id,
                kind,
                host,
                enabled,
            })
        })
        .collect()
}

async fn list(_: AuthUser, State(state): State<AppState>) -> AppResult<Json<Vec<Target>>> {
    Ok(Json(load(&state).await?))
}

#[derive(Deserialize)]
struct CreateTarget {
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    bot_token: String,
    #[serde(default)]
    chat_id: String,
}

fn telegram_destination(token: &str, chat: &str) -> AppResult<String> {
    let valid_token = token.split_once(':').is_some_and(|(bot, secret)| {
        !bot.is_empty()
            && bot.bytes().all(|b| b.is_ascii_digit())
            && !secret.is_empty()
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    });
    if !valid_token || token.len() > 256 {
        return Err(AppError::validation("Enter a valid Telegram bot token"));
    }
    let valid_channel = chat.strip_prefix('@').is_some_and(|name| {
        !name.is_empty()
            && name.len() <= 32
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    });
    if !valid_channel
        && !chat
            .parse::<i64>()
            .is_ok_and(|id| id != 0 && id.to_string() == chat)
    {
        return Err(AppError::validation(
            "Enter a numeric chat ID or @channel name",
        ));
    }
    Ok(format!(
        "https://api.telegram.org/bot{token}/sendMessage?chat_id={chat}"
    ))
}

async fn create(
    _: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTarget>,
) -> AppResult<StatusCode> {
    if !KINDS.contains(&req.kind.as_str()) {
        return Err(AppError::validation(
            "kind must be discord, slack, telegram, or generic",
        ));
    }
    let telegram_url;
    let url = if req.kind == "telegram" {
        telegram_url = telegram_destination(req.bot_token.trim(), req.chat_id.trim())?;
        telegram_url.as_str()
    } else {
        req.url.trim()
    };
    let valid = (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() <= 2048
        && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if !valid {
        return Err(AppError::validation("url must be an http(s) URL"));
    }
    let id = new_id();
    let url_enc = state
        .cipher
        .encrypt(url, &aad("notification_targets", &id, "url_enc"))?;
    sqlx::query("INSERT INTO notification_targets (id, kind, url_enc) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(&req.kind)
        .bind(url_enc)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::CREATED)
}

async fn remove(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query("DELETE FROM notification_targets WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Send a test message synchronously and report the result.
async fn test(
    _: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let (kind, url_enc): (String, String) =
        sqlx::query_as("SELECT kind, url_enc FROM notification_targets WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let url = state
        .cipher
        .decrypt(&url_enc, &aad("notification_targets", &id, "url_enc"))?;
    let event = Event {
        kind: notify::TEST,
        app_id: String::new(),
        app_name: String::new(),
        from: String::new(),
        to: String::new(),
        detail: String::new(),
    };
    notify::send(&url, &notify::payload(&kind, &event))
        .await
        .map_err(|e| AppError::validation(format!("delivery failed: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}
