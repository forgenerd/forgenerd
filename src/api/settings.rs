use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Deserializer, Serialize};

use super::AppState;
use crate::auth::AuthUser;
use crate::db::{get_setting, set_setting};
use crate::deploy::routing::{self, ROOT_DOMAIN};
use crate::error::{AppError, AppResult};
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new().route("/", get(show).patch(update))
}

#[derive(Serialize)]
struct Settings {
    /// Base domain for the panel (`hero.<root>`) and exposed app hostnames.
    root_domain: Option<String>,
    panel_domain: Option<String>,
    acme_email: Option<String>,
}

async fn load(state: &AppState, email: &str) -> AppResult<Settings> {
    Ok(Settings {
        root_domain: get_setting(&state.db, ROOT_DOMAIN).await?,
        panel_domain: routing::panel_domain(&state.db).await?,
        acme_email: Some(email.to_string()),
    })
}

async fn show(auth: AuthUser, State(state): State<AppState>) -> AppResult<Json<Settings>> {
    Ok(Json(load(&state, &auth.user.email).await?))
}

/// Only the root domain is mutable. Panel domain and ACME email are derived values.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateSettings {
    #[serde(default, deserialize_with = "present")]
    root_domain: Option<Option<String>>,
}

fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// Trimmed, lowercased and validated; empty means cleared.
fn normalize_hostname(value: Option<String>) -> AppResult<Option<String>> {
    let value = value
        .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty());
    if let Some(value) = &value {
        validate::hostname(value)?;
    }
    Ok(value)
}

async fn update(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettings>,
) -> AppResult<Json<Settings>> {
    // Validate everything first, so a rejected request changes nothing.
    let root = req.root_domain.map(normalize_hostname).transpose()?;

    // The panel domain must not be an app's custom domain, nor the default domain
    // (<name>.<root>) of an existing app under the root domain in effect afterwards.
    let new_root = match &root {
        Some(r) => r.clone(),
        None => get_setting(&state.db, ROOT_DOMAIN).await?,
    };
    let new_panel = new_root.as_deref().map(|root| format!("hero.{root}"));
    if let Some(domain) = &new_panel {
        validate::hostname(domain)?;
        let taken: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM domains WHERE hostname = ?")
            .bind(domain)
            .fetch_one(&state.db)
            .await?;
        if taken > 0 {
            return Err(AppError::Conflict(format!(
                "{domain} is already used by an app"
            )));
        }
        if let Some(label) = new_root
            .as_deref()
            .and_then(|r| domain.strip_suffix(&format!(".{r}")))
        {
            let app: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps WHERE name = ?")
                .bind(label)
                .fetch_one(&state.db)
                .await?;
            if app > 0 {
                return Err(AppError::Conflict(format!(
                    "{domain} is the default domain of app {label}; choose another app name or root domain"
                )));
            }
        }
    }

    if let Some(root) = &root {
        set_setting(&state.db, ROOT_DOMAIN, root.as_deref()).await?;
    }
    routing::sync_node(&state, "local").await?;
    Ok(Json(load(&state, &auth.user.email).await?))
}
