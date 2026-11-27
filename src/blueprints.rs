//! Built-in one-click app blueprints (`blueprints/*.yml`, embedded at compile time).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

const SOURCES: &[&str] = &[
    include_str!("../blueprints/postgres.yml"),
    include_str!("../blueprints/redis.yml"),
    include_str!("../blueprints/mysql.yml"),
];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Blueprint {
    pub id: String,
    pub name: String,
    pub description: String,
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

pub fn all() -> AppResult<Vec<Blueprint>> {
    SOURCES
        .iter()
        .map(|src| {
            serde_yaml_ng::from_str(src)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid built-in blueprint: {e}")))
        })
        .collect()
}

pub fn find(id: &str) -> AppResult<Blueprint> {
    all()?
        .into_iter()
        .find(|t| t.id == id)
        .ok_or(AppError::NotFound)
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
    /// Resolve env values: fixed values as given, generated secrets freshly random.
    /// Returns `(key, value, is_secret)`.
    pub fn resolve_env(&self) -> Vec<(String, String, bool)> {
        self.env
            .iter()
            .map(|(key, spec)| {
                let value = if spec.generate {
                    generate_secret()
                } else {
                    spec.value.clone().unwrap_or_default()
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
    use crate::validate::compose::check_raw;

    #[test]
    fn built_in_blueprints_are_valid_and_pass_the_compose_policy() {
        let blueprints = all().unwrap();
        assert_eq!(blueprints.len(), 3);
        for t in &blueprints {
            let services = check_raw(&t.compose).unwrap_or_else(|e| panic!("{}: {e}", t.id));
            assert!(
                services.contains(&t.service),
                "{}: service {} missing",
                t.id,
                t.service
            );
            // Every ${VAR} the compose file uses is provided by the blueprint.
            for key in t.env.keys() {
                assert!(
                    t.compose.contains(&format!("${{{key}}}")),
                    "{}: {key} unused",
                    t.id
                );
            }
            assert!(
                t.compose.contains("${IMAGE_TAG}"),
                "{}: image tag placeholder missing",
                t.id
            );
        }
    }

    #[test]
    fn connection_strings_are_rendered() {
        let t = find("postgres").unwrap();
        let env = t.resolve_env();
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
    }
}
