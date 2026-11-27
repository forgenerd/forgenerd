//! One-click app blueprints, downloaded from the forgenerd/blueprints catalog.
//!
//! The catalog is fetched at startup and every few hours (or from the Blueprints page)
//! and cached in the data directory, so the panel keeps its last good copy across
//! restarts and network failures. Each blueprint is checked like a Compose file a user
//! pastes in (and checked again on deploy), so a catalog can never do more than a user
//! can; blueprints that fail the checks are skipped, not fatal.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::validate;

/// Rebuilt from forgenerd/blueprints `main` on every push.
pub const DEFAULT_CATALOG_URL: &str =
    "https://github.com/forgenerd/blueprints/releases/download/catalog/catalog.json";
/// Catalog format this version understands.
const SCHEMA: u32 = 1;
const CACHE_FILE: &str = "blueprints.json";
const MAX_CATALOG_BYTES: u64 = 32 * 1024 * 1024;
/// A `data:image/png;base64,...` logo (the catalog caps the PNG at 32 KiB).
const MAX_LOGO_CHARS: usize = 48 * 1024;
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Blueprint categories, in the order the Blueprints page shows them.
pub const CATEGORIES: &[&str] = &[
    "Databases",
    "Monitoring",
    "Analytics",
    "Automation",
    "Development",
    "Productivity",
    "Communication",
    "Media",
    "Files",
    "Websites",
    "Business",
    "Security",
    "Admin tools",
    "Games",
    "Other",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Blueprint {
    pub id: String,
    pub name: String,
    /// Groups blueprints on the Blueprints page.
    pub category: String,
    pub description: String,
    /// Served publicly through Caddy (web apps). Databases stay internal.
    #[serde(default)]
    pub expose: bool,
    /// Service attached to `forgenerd-internal` under the app's name.
    pub service: String,
    pub port: u16,
    pub image: String,
    pub image_tag: String,
    #[serde(default)]
    /// Environment variables are kept in blueprint order for predictable configuration UI.
    pub env: IndexMap<String, BlueprintEnv>,
    /// Connection string with `${VAR}` and `{host}` placeholders.
    pub connection: Option<String>,
    pub compose: String,
    /// `data:image/png;base64,...`
    #[serde(default)]
    pub logo: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlueprintEnv {
    pub value: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub generate: bool,
    #[serde(default = "secret_default")]
    pub secret: bool,
}

fn secret_default() -> bool {
    true
}

impl BlueprintEnv {
    /// No default and nothing to generate: the user must supply it.
    pub fn required(&self) -> bool {
        self.value.is_none() && !self.generate
    }
}

/// The blueprints currently offered, and where they came from.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub blueprints: Vec<Blueprint>,
    /// When this catalog was downloaded; None until the first successful download.
    pub updated_at: Option<DateTime<Utc>>,
    /// Why the last refresh failed (the previous catalog is kept).
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Catalog {
    current: RwLock<Arc<Snapshot>>,
}

#[derive(Deserialize)]
struct CatalogFile {
    schema: u32,
    /// Kept as raw JSON so each blueprint parses on its own (a bad one is skipped) and
    /// its settings keep their order (a generic JSON value would sort them).
    blueprints: Vec<Box<serde_json::value::RawValue>>,
}

impl Catalog {
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.read().expect("catalog lock").clone()
    }

    pub fn find(&self, id: &str) -> AppResult<Blueprint> {
        self.snapshot()
            .blueprints
            .iter()
            .find(|b| b.id == id)
            .cloned()
            .ok_or(AppError::NotFound)
    }

    /// Replace the catalog with `json`. Returns how many blueprints were accepted.
    pub fn load(&self, json: &str, updated_at: DateTime<Utc>) -> Result<usize, String> {
        let blueprints = parse(json)?;
        let count = blueprints.len();
        *self.current.write().expect("catalog lock") = Arc::new(Snapshot {
            blueprints,
            updated_at: Some(updated_at),
            error: None,
        });
        Ok(count)
    }

    fn set_error(&self, error: String) {
        let mut current = self.current.write().expect("catalog lock");
        let mut next = (**current).clone();
        next.error = Some(error);
        *current = Arc::new(next);
    }

    /// Use the copy cached by the last successful download, if any.
    pub async fn load_cached(&self, data_dir: &Path) {
        let path = data_dir.join(CACHE_FILE);
        let Ok(json) = tokio::fs::read_to_string(&path).await else {
            return;
        };
        let updated_at = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        match self.load(&json, updated_at) {
            Ok(count) => tracing::info!(count, "loaded cached blueprint catalog"),
            Err(error) => tracing::warn!(%error, "ignoring the cached blueprint catalog"),
        }
    }

    /// Download the catalog, and cache it when it is usable. On failure the current
    /// catalog stays in place and the error is reported on the Blueprints page.
    pub async fn refresh(&self, url: &str, data_dir: &Path) -> AppResult<usize> {
        match download(url).await {
            Ok(json) => match self.load(&json, Utc::now()) {
                Ok(count) => {
                    let path = data_dir.join(CACHE_FILE);
                    let pending = data_dir.join(format!("{CACHE_FILE}.tmp"));
                    let saved = async {
                        tokio::fs::write(&pending, &json).await?;
                        tokio::fs::rename(&pending, &path).await
                    };
                    if let Err(err) = saved.await {
                        tracing::warn!(error = %err, "could not cache the blueprint catalog");
                    }
                    Ok(count)
                }
                Err(error) => {
                    self.set_error(error.clone());
                    Err(AppError::Internal(anyhow::anyhow!(
                        "blueprint catalog: {error}"
                    )))
                }
            },
            Err(error) => {
                self.set_error(error.clone());
                Err(AppError::Internal(anyhow::anyhow!(
                    "blueprint catalog: {error}"
                )))
            }
        }
    }
}

/// `FORGENERD_BLUEPRINTS_URL`, or the official catalog.
pub fn catalog_url() -> String {
    std::env::var("FORGENERD_BLUEPRINTS_URL")
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_CATALOG_URL.to_string())
}

/// Refresh the catalog now and then every few hours.
pub fn spawn_refresher(state: AppState) {
    tokio::spawn(async move {
        let url = catalog_url();
        loop {
            match state.catalog.refresh(&url, &state.data_dir).await {
                Ok(count) => tracing::info!(count, %url, "blueprint catalog updated"),
                Err(err) => tracing::warn!(error = %err, "blueprint catalog refresh failed"),
            }
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    });
}

async fn download(url: &str) -> Result<String, String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("catalog URL must be http(s): {url}"));
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("forgenerd/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let response = http
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("downloading {url}: {e}"))?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_CATALOG_BYTES)
    {
        return Err("catalog is larger than 32 MiB".into());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("downloading {url}: {e}"))?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err("catalog is larger than 32 MiB".into());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| "catalog is not UTF-8".into())
}

/// Parse a catalog, keeping the blueprints that pass [`check`] (in category, then name
/// order). Fails only when the file itself is unusable.
pub fn parse(json: &str) -> Result<Vec<Blueprint>, String> {
    let file: CatalogFile =
        serde_json::from_str(json).map_err(|e| format!("catalog is not valid: {e}"))?;
    if file.schema != SCHEMA {
        return Err(format!(
            "catalog format {} is not supported by this forgenerd version (expects {SCHEMA}); upgrade forgenerd",
            file.schema
        ));
    }
    let mut blueprints: Vec<Blueprint> = Vec::new();
    #[derive(Deserialize)]
    struct Id {
        id: Option<serde_json::Value>,
    }
    for raw in file.blueprints {
        let id = serde_json::from_str::<Id>(raw.get())
            .ok()
            .and_then(|i| i.id)
            .map_or_else(|| "?".to_string(), |v| v.to_string());
        let blueprint = match serde_json::from_str::<Blueprint>(raw.get()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(blueprint = %id, error = %e, "skipping blueprint");
                continue;
            }
        };
        if blueprints.iter().any(|b| b.id == blueprint.id) {
            tracing::warn!(blueprint = %id, "skipping duplicate blueprint");
            continue;
        }
        if let Err(e) = check(&blueprint) {
            tracing::warn!(blueprint = %id, error = %e, "skipping blueprint");
            continue;
        }
        blueprints.push(blueprint);
    }
    let rank = |b: &Blueprint| {
        CATEGORIES
            .iter()
            .position(|c| *c == b.category)
            .unwrap_or(CATEGORIES.len())
    };
    blueprints.sort_by_cached_key(|b| (rank(b), b.name.to_lowercase()));
    Ok(blueprints)
}

/// What a blueprint must satisfy to be listed. Deploying checks the Compose file again.
pub fn check(b: &Blueprint) -> AppResult<()> {
    validate::name(&b.id)?;
    let text_ok = |s: &str, max: usize| !s.trim().is_empty() && s.len() <= max;
    if !text_ok(&b.name, 100) || !text_ok(&b.category, 50) || !text_ok(&b.description, 500) {
        return Err(AppError::validation(
            "name, category and description are required",
        ));
    }
    if b.port == 0 {
        return Err(AppError::validation("port must be set"));
    }
    validate::image_tag(&b.image_tag)?;
    validate::image_ref(&format!("{}:{}", b.image, b.image_tag))?;
    if !b.compose.contains("${IMAGE_TAG}") {
        return Err(AppError::validation("compose does not use ${IMAGE_TAG}"));
    }
    for (key, spec) in &b.env {
        validate::env_key(key)?;
        if !b.expose && spec.value.as_deref().is_some_and(|v| v.contains("{url}")) {
            return Err(AppError::validation(format!(
                "{key} uses {{url}} but the app is not exposed"
            )));
        }
    }
    let services = validate::compose::check_raw(&b.compose)?;
    if !services.contains(&b.service) {
        return Err(AppError::validation(format!(
            "service {} is not in the compose file",
            b.service
        )));
    }
    if let Some(logo) = &b.logo
        && (!logo.starts_with("data:image/png;base64,") || logo.len() > MAX_LOGO_CHARS)
    {
        return Err(AppError::validation("logo must be a small inline PNG"));
    }
    Ok(())
}

/// 32 alphanumeric characters: safe inside connection strings without escaping.
pub fn generate_secret() -> String {
    use rand::RngExt;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..32)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

impl Blueprint {
    /// Resolve env values: fixed values as given (with `{url}` replaced by the app's
    /// public URL), generated secrets freshly random. Returns `(key, value, is_secret)`.
    pub fn resolve_env(&self, url: &str) -> Vec<(String, String, bool)> {
        self.env
            .iter()
            .map(|(key, spec)| {
                let value = if spec.generate {
                    generate_secret()
                } else {
                    spec.value.clone().unwrap_or_default().replace("{url}", url)
                };
                (key.clone(), value, spec.secret || spec.generate)
            })
            .collect()
    }

    pub fn connection_string(&self, host: &str, env: &[(String, String, bool)]) -> Option<String> {
        let mut out = self.connection.clone()?.replace("{host}", host);
        for (key, value, _) in env {
            out = out.replace(&format!("${{{key}}}"), value);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: &str = r#"{"id":"postgres","name":"PostgreSQL","category":"Databases","description":"d","service":"postgres","port":5432,"image":"postgres","image_tag":"18-alpine",
        "env":{"POSTGRES_USER":{"value":"app","description":"u","secret":false},"POSTGRES_PASSWORD":{"description":"p","generate":true}},
        "connection":"postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@{host}:5432/app",
        "compose":"services:\n  postgres:\n    image: postgres:${IMAGE_TAG}\n"}"#;

    fn catalog(blueprints: &[&str]) -> String {
        format!(r#"{{"schema":1,"blueprints":[{}]}}"#, blueprints.join(","))
    }

    #[test]
    fn unusable_blueprints_are_skipped_not_fatal() {
        let host_mount = PG
            .replace(r#""id":"postgres""#, r#""id":"sneaky""#)
            .replace("${IMAGE_TAG}\\n", "${IMAGE_TAG}\\n    privileged: true\\n");
        let bad_name = PG.replace(r#""id":"postgres""#, r#""id":"Bad Name""#);
        let unknown_shape = r#"{"id":"broken"}"#;
        let parsed = parse(&catalog(&[PG, &host_mount, &bad_name, unknown_shape, PG])).unwrap();
        let ids: Vec<&str> = parsed.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            ["postgres"],
            "privileged, invalid, malformed and duplicate are dropped"
        );
    }

    #[test]
    fn newer_catalog_formats_are_refused() {
        let err = parse(r#"{"schema":2,"blueprints":[]}"#).unwrap_err();
        assert!(err.contains("upgrade forgenerd"), "{err}");
        assert!(parse("not json").is_err());
    }

    #[test]
    fn connection_strings_and_urls_are_rendered() {
        let catalog_json = catalog(&[PG]);
        let t = parse(&catalog_json).unwrap().remove(0);
        let env = t.resolve_env("https://db.example.com");
        let password = &env
            .iter()
            .find(|(k, ..)| k == "POSTGRES_PASSWORD")
            .unwrap()
            .1;
        assert_eq!(password.len(), 32);
        assert!(
            env.iter()
                .all(|(k, _, secret)| (k == "POSTGRES_PASSWORD") == *secret)
        );
        let url = t.connection_string("main-db", &env).unwrap();
        assert_eq!(url, format!("postgres://app:{password}@main-db:5432/app"));
        assert_ne!(generate_secret(), generate_secret());
        assert!(!t.env["POSTGRES_USER"].required());
        let keys: Vec<&str> = t.env.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["POSTGRES_USER", "POSTGRES_PASSWORD"],
            "settings keep catalog order"
        );
    }
}
