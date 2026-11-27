use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::{AppState, ClientIp, ViaProxy};
use crate::auth::{self, AuthUser, MIN_PASSWORD_LEN, SESSION_COOKIE, SESSION_TTL_DAYS};
use crate::db::models::{ApiToken, User};
use crate::db::new_id;
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/setup", post(setup))
        .route("/status", get(status))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/password", post(change_password))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", delete(delete_token))
}

#[derive(Serialize)]
struct Status {
    setup_required: bool,
    version: &'static str,
}

/// Unauthenticated: tells the UI whether to show the setup or login screen.
async fn status(State(state): State<AppState>) -> AppResult<Json<Status>> {
    Ok(Json(Status {
        setup_required: user_count(&state).await? == 0,
        version: crate::maintenance::VERSION,
    }))
}

#[derive(Deserialize)]
struct SetupRequest {
    setup_token: String,
    email: String,
    password: String,
}

async fn setup(
    State(state): State<AppState>,
    Json(req): Json<SetupRequest>,
) -> AppResult<(StatusCode, Json<User>)> {
    // Held for the whole request: concurrent setups are serialized.
    let mut token = state.setup_token.lock().await;
    if user_count(&state).await? > 0 {
        return Err(AppError::Conflict("setup already completed".into()));
    }
    let valid = token
        .as_deref()
        .is_some_and(|t| bool::from(t.as_bytes().ct_eq(req.setup_token.as_bytes())));
    if !valid {
        return Err(AppError::Unauthorized);
    }
    let email = normalize_email(&req.email)?;
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::validation(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }

    let hash = auth::hash_password(req.password).await?;
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (id, email, password_hash, role) VALUES (?, ?, ?, 'admin') RETURNING *",
    )
    .bind(new_id())
    .bind(&email)
    .bind(hash)
    .fetch_one(&state.db)
    .await?;
    *token = None;
    tracing::info!(email = %user.email, "admin account created; setup token invalidated");
    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

/// Without connection info (in-process tests) every caller shares one address.
fn client_ip(client: Option<Extension<ClientIp>>) -> std::net::IpAddr {
    client.map_or(
        std::net::IpAddr::from([0, 0, 0, 0]),
        |Extension(ClientIp(ip))| ip,
    )
}

async fn login(
    State(state): State<AppState>,
    via_proxy: Option<Extension<ViaProxy>>,
    client: Option<Extension<ClientIp>>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> AppResult<(CookieJar, Json<User>)> {
    let ip = client_ip(client);
    if let Err(wait) = state.login_throttle.check(ip) {
        tracing::warn!(%ip, "login refused: too many failed attempts");
        return Err(AppError::TooManyRequests(wait.as_secs()));
    }
    let email = req.email.trim().to_ascii_lowercase();
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = ?")
        .bind(&email)
        .fetch_optional(&state.db)
        .await?;
    let valid = match &user {
        Some(user) => auth::verify_password(req.password, user.password_hash.clone()).await?,
        None => false,
    };
    let Some(user) = user.filter(|_| valid) else {
        state.login_throttle.record_failure(ip);
        tracing::warn!(%ip, "failed login");
        return Err(AppError::Unauthorized);
    };
    state.login_throttle.reset(ip);

    let token = auth::create_session(&state.db, &user.id).await?;
    // Requests on the gateway listener were proxied by Caddy over HTTPS; the loopback
    // listener is plain HTTP (SSH tunnel), where a `Secure` cookie would be dropped.
    let cookie = Cookie::build((SESSION_COOKIE, token))
        .secure(via_proxy.is_some())
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time_days(SESSION_TTL_DAYS))
        .build();
    Ok((jar.add(cookie), Json(user)))
}

async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<(CookieJar, StatusCode)> {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        auth::delete_session(&state.db, cookie.value()).await?;
    }
    let removal = Cookie::build((SESSION_COOKIE, "")).path("/").build();
    Ok((jar.remove(removal), StatusCode::NO_CONTENT))
}

async fn me(auth: AuthUser) -> Json<User> {
    Json(auth.user)
}

#[derive(Deserialize)]
struct ChangePassword {
    current_password: String,
    new_password: String,
}

/// Change the caller's password and sign out every other session. API tokens keep
/// working; revoke them separately.
async fn change_password(
    auth: AuthUser,
    State(state): State<AppState>,
    client: Option<Extension<ClientIp>>,
    Json(req): Json<ChangePassword>,
) -> AppResult<StatusCode> {
    let ip = client_ip(client);
    if let Err(wait) = state.login_throttle.check(ip) {
        return Err(AppError::TooManyRequests(wait.as_secs()));
    }
    if !auth::verify_password(req.current_password, auth.user.password_hash.clone()).await? {
        state.login_throttle.record_failure(ip);
        return Err(AppError::validation("current password is incorrect"));
    }
    if req.new_password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::validation(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let hash = auth::hash_password(req.new_password).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(&auth.user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash IS NOT ?")
        .bind(&auth.user.id)
        .bind(&auth.session_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(email = %auth.user.email, "password changed; other sessions signed out");
    Ok(StatusCode::NO_CONTENT)
}

async fn list_tokens(
    auth: AuthUser,
    State(state): State<AppState>,
) -> AppResult<Json<Vec<ApiToken>>> {
    let tokens = sqlx::query_as::<_, ApiToken>(
        "SELECT id, name, created_at FROM api_tokens WHERE user_id = ? ORDER BY created_at",
    )
    .bind(&auth.user.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(tokens))
}

#[derive(Deserialize)]
struct CreateTokenRequest {
    name: String,
}

#[derive(Serialize)]
struct CreatedToken {
    #[serde(flatten)]
    meta: ApiToken,
    /// Shown exactly once; only its hash is stored.
    token: String,
}

async fn create_token(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTokenRequest>,
) -> AppResult<(StatusCode, Json<CreatedToken>)> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::validation("token name must be 1-100 characters"));
    }
    let token = auth::new_token();
    let meta = sqlx::query_as::<_, ApiToken>(
        "INSERT INTO api_tokens (id, user_id, name, token_hash) VALUES (?, ?, ?, ?) \
         RETURNING id, name, created_at",
    )
    .bind(new_id())
    .bind(&auth.user.id)
    .bind(name)
    .bind(auth::token_hash(&token))
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedToken { meta, token })))
}

async fn delete_token(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query("DELETE FROM api_tokens WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(&auth.user.id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn user_count(state: &AppState) -> AppResult<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await?)
}

fn normalize_email(raw: &str) -> AppResult<String> {
    let email = raw.trim().to_ascii_lowercase();
    let valid = email.len() <= 254
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        })
        && !email.chars().any(char::is_whitespace);
    if valid {
        Ok(email)
    } else {
        Err(AppError::validation("invalid email address"))
    }
}

fn time_days(days: i64) -> cookie::time::Duration {
    cookie::time::Duration::days(days)
}
