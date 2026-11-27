use chrono::NaiveDateTime;
use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip)]
    pub password_hash: String,
    pub role: String,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub public_ip: Option<String>,
    pub status: String,
    pub last_seen_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub created_at: NaiveDateTime,
}

/// Columns of `apps` that are safe to load and return (no encrypted secrets, no compose body).
/// A macro so queries can embed it with `concat!` and stay static strings.
#[macro_export]
macro_rules! app_columns {
    () => {
        "id, project_id, node_id, name, deploy_type, image_name, git_repo_url, git_branch, \
         build_method, compose_service, container_port, healthcheck_path, active_container_id, \
         active_deployment_id, status, exposed, memory_limit_mb, cpu_limit, health_timeout_secs, \
         created_at, updated_at"
    };
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct App {
    pub id: String,
    pub project_id: String,
    pub node_id: String,
    pub name: String,
    pub deploy_type: String,
    pub image_name: Option<String>,
    pub git_repo_url: Option<String>,
    pub git_branch: Option<String>,
    pub build_method: Option<String>,
    pub compose_service: Option<String>,
    pub container_port: i64,
    pub healthcheck_path: Option<String>,
    pub active_container_id: Option<String>,
    pub active_deployment_id: Option<String>,
    pub status: String,
    /// Served publicly through the proxy (default domain plus custom domains).
    pub exposed: bool,
    /// Hard memory limit in MiB; `None` = unlimited.
    pub memory_limit_mb: Option<i64>,
    /// CPU limit in cores; `None` = unlimited.
    pub cpu_limit: Option<f64>,
    /// Health check deadline for new deployments; `None` = the default.
    pub health_timeout_secs: Option<i64>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl App {
    /// How long a new deployment may take to pass its health check.
    pub fn health_timeout(&self, default: std::time::Duration) -> std::time::Duration {
        self.health_timeout_secs
            .and_then(|s| u64::try_from(s).ok())
            .map_or(default, std::time::Duration::from_secs)
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Domain {
    pub id: String,
    pub hostname: String,
    pub force_https: bool,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct EnvVarRow {
    pub id: String,
    pub key: String,
    pub value_enc: String,
    pub is_secret: bool,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Volume {
    pub id: String,
    pub name: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Deployment {
    pub id: String,
    pub app_id: String,
    pub commit_sha: Option<String>,
    pub image_ref: Option<String>,
    pub status: String,
    pub started_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
}

#[macro_export]
macro_rules! deployment_columns {
    () => {
        "id, app_id, commit_sha, image_ref, status, started_at, finished_at"
    };
}
