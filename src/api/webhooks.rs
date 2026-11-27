//! `POST /webhooks/git/{app_id}`: push webhooks from GitHub, Gitea/Forgejo (HMAC-SHA256
//! of the body in `X-Hub-Signature-256` / `X-Gitea-Signature`) and GitLab (shared token in
//! `X-Gitlab-Token`). Unauthenticated by session; the per-app secret is the credential.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::AppState;
use crate::auth::crypto::aad;
use crate::deploy::pipeline::{self, Trigger};
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new().route("/git/{app_id}", post(git_push))
}

/// Verify the request against the app's secret. Any failure is reported as 404 so the
/// endpoint does not reveal which app ids exist.
pub fn verify(headers: &HeaderMap, body: &[u8], secret: &str) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(token) = header("x-gitlab-token") {
        return bool::from(token.as_bytes().ct_eq(secret.as_bytes()));
    }
    let signature = header("x-hub-signature-256")
        .map(|s| s.strip_prefix("sha256=").unwrap_or(s))
        .or_else(|| header("x-gitea-signature"));
    let Some(Ok(signature)) = signature.map(hex::decode) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

async fn git_push(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<Value>)> {
    let row: Option<(Option<String>, String)> =
        sqlx::query_as("SELECT webhook_secret_enc, deploy_type FROM apps WHERE id = ?")
            .bind(&app_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((Some(secret_enc), deploy_type)) = row else {
        return Err(AppError::NotFound);
    };
    if deploy_type != "git" {
        return Err(AppError::NotFound);
    }
    let secret = state
        .cipher
        .decrypt(&secret_enc, &aad("apps", &app_id, "webhook_secret_enc"))?;
    if !verify(&headers, &body, &secret) {
        tracing::warn!(app_id, "git webhook rejected: bad signature");
        return Err(AppError::NotFound);
    }

    let event = headers
        .get("x-github-event")
        .or_else(|| headers.get("x-gitea-event"))
        .or_else(|| headers.get("x-gitlab-event"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("push");
    if event == "ping" {
        return Ok((StatusCode::OK, Json(json!({ "ok": "pong" }))));
    }

    let payload: Value = serde_json::from_slice(&body).map_err(|_| {
        AppError::validation(
            "payload must be JSON (set the webhook content type to application/json)",
        )
    })?;
    let app = sqlx::query_as::<_, crate::db::models::App>(concat!(
        "SELECT ",
        crate::app_columns!(),
        " FROM apps WHERE id = ?"
    ))
    .bind(&app_id)
    .fetch_one(&state.db)
    .await?;
    let branch = app.git_branch.as_deref().unwrap_or("main");
    let pushed = payload["ref"].as_str().unwrap_or_default();
    if pushed != format!("refs/heads/{branch}") {
        return Ok((
            StatusCode::OK,
            Json(
                json!({ "ignored": format!("push to {pushed:?}, app tracks refs/heads/{branch}") }),
            ),
        ));
    }
    let deployment_id = pipeline::enqueue(&state, &app, Trigger::Build).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "deployment_id": deployment_id })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verifies_each_provider_style() {
        let body = br#"{"ref":"refs/heads/main"}"#;
        let mut gh = HeaderMap::new();
        gh.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&format!("sha256={}", sign("s3", body))).unwrap(),
        );
        assert!(verify(&gh, body, "s3"));
        assert!(!verify(&gh, body, "other"));
        assert!(!verify(&gh, b"tampered", "s3"));

        let mut gitea = HeaderMap::new();
        gitea.insert(
            "x-gitea-signature",
            HeaderValue::from_str(&sign("s3", body)).unwrap(),
        );
        assert!(verify(&gitea, body, "s3"));

        let mut gitlab = HeaderMap::new();
        gitlab.insert("x-gitlab-token", HeaderValue::from_static("s3"));
        assert!(verify(&gitlab, body, "s3"));
        gitlab.insert("x-gitlab-token", HeaderValue::from_static("nope"));
        assert!(!verify(&gitlab, body, "s3"));

        assert!(!verify(&HeaderMap::new(), body, "s3"));
    }
}
