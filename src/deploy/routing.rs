//! Builds a node's complete proxy state from SQLite (the single source of truth) and
//! pushes it through the node's executor. Serialized per node so concurrent changes
//! can never overwrite each other with stale route sets.

use std::collections::HashSet;

use sqlx::SqlitePool;

use crate::api::AppState;
use crate::db::get_setting;
use crate::error::AppResult;
use crate::executor::{ProxyConfig, ProxyRoute, compose_container_name, container_name};
use crate::validate;

pub const ROOT_DOMAIN: &str = "root_domain";

/// The panel hostname is fixed to the `hero` subdomain of the configured root.
pub async fn panel_domain(db: &SqlitePool) -> sqlx::Result<Option<String>> {
    Ok(get_setting(db, ROOT_DOMAIN)
        .await?
        .map(|root| format!("hero.{root}")))
}

/// The ACME contact is the email used to register the admin account.
pub async fn acme_email(db: &SqlitePool) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT email FROM users ORDER BY created_at LIMIT 1")
        .fetch_optional(db)
        .await
}

/// Decides each exposed app's default hostname, `<name>.<root_domain>`. Hostnames that
/// are already the panel domain or someone's custom domain are never a default: an
/// explicit claim always wins over a derived one.
pub struct PublicNames {
    root: Option<String>,
    reserved: HashSet<String>,
}

impl PublicNames {
    pub async fn load(db: &SqlitePool) -> sqlx::Result<Self> {
        let mut reserved: HashSet<String> = sqlx::query_scalar("SELECT hostname FROM domains")
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
        reserved.extend(panel_domain(db).await?);
        Ok(Self {
            root: get_setting(db, ROOT_DOMAIN).await?,
            reserved,
        })
    }

    /// `<name>.<root>`, whether or not something else already claims it.
    pub fn candidate(&self, app_name: &str) -> Option<String> {
        let root = self.root.as_deref()?;
        let hostname = format!("{app_name}.{root}");
        validate::hostname(&hostname).ok().map(|()| hostname)
    }

    /// The default hostname the app is actually served on, if any.
    pub fn default_for(&self, app_name: &str) -> Option<String> {
        self.candidate(app_name)
            .filter(|h| !self.reserved.contains(h))
    }

    /// Every public hostname of an app: the default one first, then its custom domains.
    /// Empty when the app is not exposed.
    pub fn hostnames(&self, app_name: &str, exposed: bool, custom: &[String]) -> Vec<String> {
        if !exposed {
            return vec![];
        }
        self.default_for(app_name)
            .into_iter()
            .chain(custom.iter().cloned())
            .collect()
    }
}

#[derive(sqlx::FromRow)]
struct RoutedApp {
    id: String,
    name: String,
    active_deployment_id: String,
    container_port: i64,
    deploy_type: String,
    compose_service: Option<String>,
}

pub async fn sync_node(state: &AppState, node_id: &str) -> AppResult<()> {
    let lock = state.proxy_locks.get(node_id);
    let _guard = lock.lock().await;

    let apps = sqlx::query_as::<_, RoutedApp>(
        "SELECT id, name, active_deployment_id, container_port, deploy_type, compose_service \
         FROM apps WHERE node_id = ? AND active_deployment_id IS NOT NULL AND exposed = 1 \
         ORDER BY name",
    )
    .bind(node_id)
    .fetch_all(&state.db)
    .await?;
    let custom: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT d.app_id, d.hostname, d.force_https FROM domains d \
         JOIN apps a ON a.id = d.app_id WHERE a.node_id = ? ORDER BY d.hostname",
    )
    .bind(node_id)
    .fetch_all(&state.db)
    .await?;
    let names = PublicNames::load(&state.db).await?;

    let mut routes = Vec::new();
    for app in apps {
        let container = if app.deploy_type == "compose" {
            // Compose apps are routed through their exposed service only.
            let Some(service) = app.compose_service.as_deref() else {
                continue;
            };
            compose_container_name(&app.name, service)
        } else {
            container_name(&app.name, &app.active_deployment_id)
        };
        let upstream = format!("{container}:{}", app.container_port);
        let hosts = names
            .default_for(&app.name)
            .map(|h| (h, true))
            .into_iter()
            .chain(
                custom
                    .iter()
                    .filter(|(id, _, _)| *id == app.id)
                    .map(|(_, h, https)| (h.clone(), *https)),
            );
        for (hostname, force_https) in hosts {
            routes.push(ProxyRoute {
                hostnames: vec![hostname],
                upstream: upstream.clone(),
                force_https,
            });
        }
    }
    routes.sort_by(|a, b| a.hostnames.cmp(&b.hostnames));

    // The panel is served by the control plane's own node.
    let (panel_hostname, acme_email) = if node_id == "local" {
        (panel_domain(&state.db).await?, acme_email(&state.db).await?)
    } else {
        (None, None)
    };

    let config = ProxyConfig {
        routes,
        panel_hostname,
        acme_email,
    };
    state
        .registry
        .get(node_id)?
        .sync_proxy_routes(&config)
        .await
}
