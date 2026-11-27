//! Placeholder for the v1.1 `RemoteAgentExecutor`, which will forward each
//! `NodeExecutor` call over an outbound WebSocket opened by `forgenerd agent`.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::*;

pub struct RemoteAgentExecutor;

#[async_trait]
impl NodeExecutor for RemoteAgentExecutor {
    async fn ensure_node_ready(&self) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn prepare_image(
        &self,
        _: &BuildSpec,
        _: mpsc::Sender<String>,
    ) -> Result<PreparedImage, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn start_container(&self, _: &ContainerSpec) -> Result<RunningContainer, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn wait_healthy(&self, _: &HealthTarget, _: Duration) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn sync_proxy_routes(&self, _: &ProxyConfig) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn publish_alias(&self, _: &str, _: &str) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn probe_certificates(&self, _: &[String]) -> Result<Vec<CertStatus>, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn remove_volumes(&self, _: &[String]) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn remove_container(&self, _: &str) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn set_container_state(&self, _: &str, _: ContainerAction) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn deploy_compose(
        &self,
        _: &ComposeSpec,
        _: mpsc::Sender<String>,
    ) -> Result<Option<RunningContainer>, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn remove_compose(&self, _: &str, _: bool) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn stream_logs(&self, _: &str) -> Result<LogStream, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerHealth>, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn prune_images(&self, _: &str, _: &[String]) -> Result<Vec<String>, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn prune_unused(&self) -> Result<u64, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn run_backup(&self, _: &BackupSpec) -> Result<BackupResult, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn run_restore(&self, _: &RestoreSpec) -> Result<(), AppError> {
        Err(AppError::NotImplemented)
    }
    async fn container_stats(&self, _: &[String]) -> Result<Vec<ContainerStats>, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn disk_usage(&self) -> Result<DiskUsage, AppError> {
        Err(AppError::NotImplemented)
    }
}
