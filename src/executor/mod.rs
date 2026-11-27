//! `NodeExecutor`: the only boundary through which the control plane touches a node's
//! Docker engine, `docker compose` CLI, or Caddy.
//! All DTOs are serde-serializable so a future `RemoteAgentExecutor` can ship them as-is.

pub mod local;
pub mod remote;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::AppError;

pub type LogStream = mpsc::Receiver<String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildSpec {
    Pull {
        image: String,
    },
    Git {
        app_id: String,
        app_name: String,
        deployment_id: String,
        repo_url: String,
        branch: String,
        /// Decrypted deploy key or access token, if the repo is private.
        credential: Option<String>,
        method: BuildMethod,
    },
}

/// Result of `prepare_image`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedImage {
    pub image_ref: String,
    /// Commit that was built (git builds only).
    pub commit_sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildMethod {
    Dockerfile,
    Railpack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMount {
    pub volume_name: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub app_id: String,
    pub app_name: String,
    pub deployment_id: String,
    pub image: String,
    /// Decrypted environment variables.
    pub env: BTreeMap<String, String>,
    pub container_port: u16,
    pub volumes: Vec<VolumeMount>,
    /// Hard memory limit; the container is OOM-killed above it. `None` = unlimited.
    pub memory_limit_mb: Option<u32>,
    /// CPU limit in cores (e.g. 0.5). `None` = unlimited.
    pub cpu_limit: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningContainer {
    pub id: String,
    pub name: String,
    /// Address on `forgenerd-internal`, reachable from the node host.
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthTarget {
    /// Container being checked; an exited container fails the check immediately.
    pub container_id: String,
    /// Address reachable from the node host (the container's `forgenerd-internal` IP).
    pub host: String,
    pub port: u16,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyRoute {
    pub hostnames: Vec<String>,
    /// `host:port` dialed by Caddy over `forgenerd-internal`.
    pub upstream: String,
    pub force_https: bool,
}

/// The complete desired proxy state of a node. `sync_proxy_routes` replaces everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub routes: Vec<ProxyRoute>,
    /// Hostname that serves the forgenerd panel itself (local node only).
    pub panel_hostname: Option<String>,
    /// Contact email for ACME (Let's Encrypt) accounts.
    pub acme_email: Option<String>,
}

/// Container name convention: `forgenerd-<app_name>-<short_deployment_id>`. The short id
/// uses the random tail of the UUIDv7 (its head is a timestamp and repeats within a minute).
pub fn container_name(app_name: &str, deployment_id: &str) -> String {
    let hex: String = deployment_id.chars().filter(|c| *c != '-').collect();
    let short = &hex[hex.len().saturating_sub(12)..];
    format!("forgenerd-{app_name}-{short}")
}

/// Named Docker volume for an app volume: `forgenerd-<app_name>-<volume_name>`.
pub fn volume_name(app_name: &str, volume: &str) -> String {
    format!("forgenerd-{app_name}-{volume}")
}

/// Resolves the executor responsible for a node.
#[derive(Default)]
pub struct NodeRegistry {
    nodes: HashMap<String, Arc<dyn NodeExecutor>>,
}

impl NodeRegistry {
    pub fn insert(&mut self, node_id: impl Into<String>, executor: Arc<dyn NodeExecutor>) {
        self.nodes.insert(node_id.into(), executor);
    }

    pub fn ids(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    pub fn get(&self, node_id: &str) -> Result<Arc<dyn NodeExecutor>, AppError> {
        self.nodes.get(node_id).cloned().ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("no executor registered for node {node_id}"))
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeSpec {
    pub app_id: String,
    pub app_name: String,
    pub deployment_id: String,
    pub compose_yaml: String,
    /// Decrypted app env vars: the ONLY environment `docker compose` sees (for `${VAR}`
    /// interpolation); forgenerd's own environment is never passed through.
    pub env: BTreeMap<String, String>,
    /// Service attached to `forgenerd-internal` (alias = app name), reachable by Caddy
    /// and by other apps. `None` keeps the whole stack private.
    pub expose_service: Option<String>,
    /// How long `docker compose up --wait` may take for services to become healthy.
    pub wait_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertState {
    /// A certificate from a public CA (Let's Encrypt, ZeroSSL) is being served.
    Issued,
    /// Caddy's internal CA (e.g. `.localhost` names): valid locally, not publicly trusted.
    LocalCa,
    Expired,
    /// No certificate yet (usually DNS does not point at this server yet).
    Pending,
    /// The proxy could not be reached or the name is invalid.
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertStatus {
    pub hostname: String,
    pub state: CertState,
    pub issuer: Option<String>,
    pub not_after: Option<chrono::DateTime<chrono::Utc>>,
    pub detail: Option<String>,
}

/// Compose project name for an app: `forgenerd-<app_name>`.
pub fn compose_project(app_name: &str) -> String {
    format!("forgenerd-{app_name}")
}

/// Container name of a compose service's first replica.
pub fn compose_container_name(app_name: &str, service: &str) -> String {
    format!("{}-{service}-1", compose_project(app_name))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerHealth {
    pub id: String,
    pub app_id: String,
    /// Docker container name. Compose services use this to map runtime state back
    /// to the service declared in the stored compose file.
    pub name: String,
    /// Empty for compose containers: they are not relabelled per deployment, so that
    /// `docker compose up` only recreates services whose config actually changed.
    pub deployment_id: String,
    pub running: bool,
    pub restarting: bool,
    pub restart_count: i64,
    pub health: Option<String>,
    /// Address on `forgenerd-internal`, reachable from the node host (for HTTP probes).
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Destination {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// e.g. `backups/<app_name>/<job_id>/`; retention applies to objects under it.
    pub prefix: String,
    pub retention_count: u32,
}

/// What to back up. Database credentials are optional: when absent, the dump runs with
/// the container's own environment (e.g. `POSTGRES_USER`/`POSTGRES_DB` of the blueprints).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case")]
pub enum BackupSource {
    Postgres {
        container: String,
        db_name: Option<String>,
        db_user: Option<String>,
        db_password: Option<String>,
    },
    Mysql {
        container: String,
        db_name: Option<String>,
        db_user: Option<String>,
        db_password: Option<String>,
    },
    Volume {
        volume_name: String,
    },
}

impl BackupSource {
    pub fn driver(&self) -> &'static str {
        match self {
            Self::Postgres { .. } => "postgres",
            Self::Mysql { .. } => "mysql",
            Self::Volume { .. } => "volume",
        }
    }

    /// File extension of the produced archive.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Postgres { .. } => "dump",
            Self::Mysql { .. } => "sql.gz",
            Self::Volume { .. } => "tar.gz",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSpec {
    pub source: BackupSource,
    pub destination: S3Destination,
    /// Object key to upload to (under `destination.prefix`).
    pub object_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupResult {
    pub object_key: String,
    pub size_bytes: u64,
    /// Older objects under the prefix deleted by retention.
    pub pruned: Vec<String>,
}

/// Restore `object_key` from S3 into `target`. `destination.prefix` and
/// `destination.retention_count` are unused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSpec {
    pub target: BackupSource,
    pub source: S3Destination,
    pub object_key: String,
}

/// Point-in-time resource usage of one container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStats {
    pub id: String,
    pub name: String,
    /// Percent of one core; can exceed 100 on multi-core hosts.
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    /// The container's memory limit, or the host's memory when unlimited.
    pub memory_limit_bytes: u64,
}

/// Disk used by Docker and free space on the filesystem holding forgenerd's data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskUsage {
    pub images_bytes: u64,
    pub images_reclaimable_bytes: u64,
    pub containers_bytes: u64,
    pub volumes_bytes: u64,
    pub build_cache_bytes: u64,
    pub filesystem_total_bytes: u64,
    pub filesystem_free_bytes: u64,
}

#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Ensure `forgenerd-internal` network and the `forgenerd-caddy` container exist.
    async fn ensure_node_ready(&self) -> Result<(), AppError>;

    /// Build or pull the target image, streaming log lines to `log_tx`.
    async fn prepare_image(
        &self,
        spec: &BuildSpec,
        log_tx: mpsc::Sender<String>,
    ) -> Result<PreparedImage, AppError>;

    /// Start a candidate container on `forgenerd-internal` (no published host ports).
    async fn start_container(&self, spec: &ContainerSpec) -> Result<RunningContainer, AppError>;

    /// Poll the HTTP healthcheck until healthy or `timeout`.
    async fn wait_healthy(&self, target: &HealthTarget, timeout: Duration) -> Result<(), AppError>;

    /// Replace the node's complete proxy config with the FULL desired state.
    async fn sync_proxy_routes(&self, config: &ProxyConfig) -> Result<(), AppError>;

    /// Re-attach a healthy container to `forgenerd-internal` with `alias` (the app name),
    /// so other apps reach it at a stable hostname. Called only after the health gate,
    /// so a failing candidate never answers on the app's name.
    async fn publish_alias(&self, container_id: &str, alias: &str) -> Result<(), AppError>;

    /// Gracefully stop and remove a container.
    async fn remove_container(&self, container_id: &str) -> Result<(), AppError>;

    /// Certificate state of each hostname as served by this node's proxy.
    async fn probe_certificates(&self, hostnames: &[String]) -> Result<Vec<CertStatus>, AppError>;

    /// Delete named volumes (missing volumes are ignored).
    async fn remove_volumes(&self, names: &[String]) -> Result<(), AppError>;

    async fn set_container_state(
        &self,
        container_id: &str,
        action: ContainerAction,
    ) -> Result<(), AppError>;

    /// Validate, normalize and `docker compose up --wait` a stack. Returns the exposed
    /// service's container, if any.
    async fn deploy_compose(
        &self,
        spec: &ComposeSpec,
        log_tx: mpsc::Sender<String>,
    ) -> Result<Option<RunningContainer>, AppError>;

    async fn remove_compose(&self, project: &str, remove_volumes: bool) -> Result<(), AppError>;

    async fn stream_logs(&self, container_id: &str) -> Result<LogStream, AppError>;

    async fn list_managed_containers(&self) -> Result<Vec<ContainerHealth>, AppError>;

    /// Remove the app's forgenerd images except `keep`; returns the removed tags.
    async fn prune_images(&self, app_name: &str, keep: &[String]) -> Result<Vec<String>, AppError>;

    /// Remove dangling images and unused build cache; returns the bytes reclaimed.
    async fn prune_unused(&self) -> Result<u64, AppError>;

    /// Dump/archive on the node, upload to S3 directly, prune by retention. Temporary
    /// files never leave the node.
    async fn run_backup(&self, spec: &BackupSpec) -> Result<BackupResult, AppError>;

    /// Download a backup from S3 on the node and restore it into its target.
    async fn run_restore(&self, spec: &RestoreSpec) -> Result<(), AppError>;

    /// Current CPU and memory usage of the given containers (missing ones are skipped).
    async fn container_stats(
        &self,
        container_ids: &[String],
    ) -> Result<Vec<ContainerStats>, AppError>;

    async fn disk_usage(&self) -> Result<DiskUsage, AppError>;
}
