//! v1 `NodeExecutor` for the host forgenerd runs on: bollard + the Caddy container.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use bollard::Docker;
use tokio::sync::mpsc;

use super::*;
use crate::backup::s3::S3Client;
use crate::docker::builder::{self, BuildRequest};
use crate::docker::{self, container};
use crate::proxy::caddy::{self, CaddyPaths};
use std::os::unix::fs::PermissionsExt;

pub struct LocalDockerExecutor {
    docker: Docker,
    http: reqwest::Client,
    caddy: CaddyPaths,
    builds_dir: std::path::PathBuf,
    compose_dir: std::path::PathBuf,
    backups_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    /// One image build at a time per node: concurrent builds OOM a 1 GB VPS.
    build_permit: tokio::sync::Semaphore,
    panel_port: u16,
    /// Host address on `forgenerd-internal`, known after `ensure_node_ready`.
    gateway: RwLock<Option<IpAddr>>,
}

impl LocalDockerExecutor {
    pub fn new(data_dir: &Path, panel_port: u16) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            docker: docker::connect()?,
            http,
            caddy: CaddyPaths::new(data_dir)?,
            builds_dir: builder::builds_dir(data_dir),
            compose_dir: docker::compose::compose_dir(data_dir),
            backups_dir: data_dir.join("backups"),
            data_dir: data_dir.to_path_buf(),
            build_permit: tokio::sync::Semaphore::new(1),
            panel_port,
            gateway: RwLock::new(None),
        })
    }

    /// A fresh path in the private (0700) backup temp directory.
    async fn temp_file(&self, extension: &str) -> Result<std::path::PathBuf, AppError> {
        let tmp_dir = self.backups_dir.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir)
            .await
            .map_err(|e| internal(e.into()))?;
        let _ = std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700));
        Ok(tmp_dir.join(format!("{}.{extension}", uuid::Uuid::now_v7().simple())))
    }

    /// The host's address on `forgenerd-internal`; the panel listens there for Caddy.
    pub fn gateway_ip(&self) -> Option<IpAddr> {
        self.gateway.read().ok().and_then(|g| *g)
    }
}

fn internal(err: anyhow::Error) -> AppError {
    AppError::Internal(err)
}

#[async_trait]
impl NodeExecutor for LocalDockerExecutor {
    async fn ensure_node_ready(&self) -> Result<(), AppError> {
        let gateway = docker::ensure_network(&self.docker)
            .await
            .map_err(internal)?;
        if let Ok(mut slot) = self.gateway.write() {
            *slot = Some(gateway);
        }
        caddy::ensure_container(&self.docker, &self.caddy)
            .await
            .map_err(internal)?;
        // Caddy needs a moment to open its admin socket after (re)starting. Wait for a
        // live connection: a stale socket file from a previous container can exist
        // before the new Caddy is listening.
        for _ in 0..100 {
            if tokio::net::UnixStream::connect(&self.caddy.socket)
                .await
                .is_ok()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(internal(anyhow!(
            "Caddy admin socket is not accepting connections at {}",
            self.caddy.socket.display()
        )))
    }

    async fn prepare_image(
        &self,
        spec: &BuildSpec,
        log_tx: mpsc::Sender<String>,
    ) -> Result<PreparedImage, AppError> {
        match spec {
            BuildSpec::Pull { image } => {
                let _ = log_tx.send(format!("pulling {image}")).await;
                let mut log = |line: String| {
                    let _ = log_tx.try_send(line);
                };
                docker::pull_image(&self.docker, image, &mut log)
                    .await
                    .map_err(internal)?;
                Ok(PreparedImage {
                    image_ref: image.clone(),
                    commit_sha: None,
                })
            }
            BuildSpec::Git {
                app_id,
                app_name,
                deployment_id,
                repo_url,
                branch,
                credential,
                method,
            } => {
                if self.build_permit.available_permits() == 0 {
                    let _ = log_tx
                        .send("waiting for another build on this node to finish".into())
                        .await;
                }
                let _permit = self
                    .build_permit
                    .acquire()
                    .await
                    .map_err(|e| internal(anyhow!(e)))?;
                let request = BuildRequest {
                    app_id,
                    app_name,
                    deployment_id,
                    repo_url,
                    branch,
                    credential: credential.as_deref(),
                    method: *method,
                };
                let output = builder::build(&self.builds_dir, &request, &log_tx)
                    .await
                    .map_err(internal)?;
                Ok(PreparedImage {
                    image_ref: output.image_ref,
                    commit_sha: Some(output.commit_sha),
                })
            }
        }
    }

    async fn start_container(&self, spec: &ContainerSpec) -> Result<RunningContainer, AppError> {
        container::start(&self.docker, spec).await.map_err(internal)
    }

    async fn wait_healthy(&self, target: &HealthTarget, timeout: Duration) -> Result<(), AppError> {
        container::wait_healthy(&self.docker, &self.http, target, timeout)
            .await
            .map_err(internal)
    }

    async fn sync_proxy_routes(&self, config: &ProxyConfig) -> Result<(), AppError> {
        let panel = self
            .gateway_ip()
            .map(|ip| SocketAddr::new(ip, self.panel_port));
        let json = caddy::build_config(config, panel);
        caddy::load(&self.caddy.socket, &json)
            .await
            .map_err(internal)
    }

    async fn remove_container(&self, container_id: &str) -> Result<(), AppError> {
        container::remove(&self.docker, container_id)
            .await
            .map_err(internal)
    }

    async fn publish_alias(&self, container_id: &str, alias: &str) -> Result<(), AppError> {
        container::publish_alias(&self.docker, container_id, alias)
            .await
            .map_err(internal)
    }

    async fn probe_certificates(&self, hostnames: &[String]) -> Result<Vec<CertStatus>, AppError> {
        // Caddy publishes 443 on the host; probe all names concurrently.
        let addr = SocketAddr::from(([127, 0, 0, 1], 443));
        let probes = hostnames
            .iter()
            .map(|h| crate::proxy::tls_probe::probe(addr, h, Duration::from_secs(5)));
        Ok(futures_util::future::join_all(probes).await)
    }

    async fn remove_volumes(&self, names: &[String]) -> Result<(), AppError> {
        container::remove_volumes(&self.docker, names)
            .await
            .map_err(internal)
    }

    async fn set_container_state(
        &self,
        container_id: &str,
        action: ContainerAction,
    ) -> Result<(), AppError> {
        container::set_state(&self.docker, container_id, action)
            .await
            .map_err(internal)
    }

    async fn deploy_compose(
        &self,
        spec: &ComposeSpec,
        log_tx: mpsc::Sender<String>,
    ) -> Result<Option<RunningContainer>, AppError> {
        docker::compose::deploy(&self.docker, &self.compose_dir, spec, &log_tx)
            .await
            .map_err(|err| match err.downcast::<AppError>() {
                Ok(app) => app,
                Err(err) => internal(err),
            })
    }
    async fn remove_compose(&self, project: &str, remove_volumes: bool) -> Result<(), AppError> {
        docker::compose::down(project, remove_volumes)
            .await
            .map_err(internal)
    }
    async fn stream_logs(&self, container_id: &str) -> Result<LogStream, AppError> {
        Ok(container::stream_logs(&self.docker, container_id))
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerHealth>, AppError> {
        container::list_managed(&self.docker)
            .await
            .map_err(internal)
    }
    async fn prune_images(&self, app_name: &str, keep: &[String]) -> Result<Vec<String>, AppError> {
        let removed = container::prune_images(&self.docker, app_name, keep)
            .await
            .map_err(internal)?;
        if !removed.is_empty() {
            tracing::info!(app = app_name, ?removed, "pruned old images");
        }
        Ok(removed)
    }
    async fn prune_unused(&self) -> Result<u64, AppError> {
        container::prune_unused(&self.docker)
            .await
            .map_err(internal)
    }
    async fn run_restore(&self, spec: &RestoreSpec) -> Result<(), AppError> {
        let src = &spec.source;
        let client = S3Client::new(
            &src.endpoint,
            &src.bucket,
            &src.region,
            &src.access_key,
            &src.secret_key,
        )
        .map_err(internal)?;
        let out = self.temp_file(spec.target.extension()).await?;
        let result = async {
            client.get_to_file(&spec.object_key, &out).await?;
            docker::backup::restore(&self.docker, &spec.target, caddy::IMAGE, &out).await
        }
        .await;
        let _ = tokio::fs::remove_file(&out).await;
        result.map_err(internal)
    }
    async fn container_stats(&self, ids: &[String]) -> Result<Vec<ContainerStats>, AppError> {
        container::stats(&self.docker, ids).await.map_err(internal)
    }
    async fn disk_usage(&self) -> Result<DiskUsage, AppError> {
        container::disk_usage(&self.docker, &self.data_dir)
            .await
            .map_err(internal)
    }
    async fn run_backup(&self, spec: &BackupSpec) -> Result<BackupResult, AppError> {
        let dest = &spec.destination;
        let client = S3Client::new(
            &dest.endpoint,
            &dest.bucket,
            &dest.region,
            &dest.access_key,
            &dest.secret_key,
        )
        .map_err(internal)?;
        let out = self.temp_file(spec.source.extension()).await?;

        let result = async {
            let archive =
                docker::backup::create(&self.docker, &spec.source, caddy::IMAGE, &out).await?;
            let size_bytes = client.put_file(&spec.object_key, &archive).await?;
            // Retention: keys sort chronologically (timestamp prefix); keep the newest N.
            let mut objects = client.list(&dest.prefix).await?;
            objects.sort_by(|a, b| a.key.cmp(&b.key));
            let excess = objects
                .len()
                .saturating_sub(dest.retention_count.max(1) as usize);
            let mut pruned = Vec::new();
            for object in objects.into_iter().take(excess) {
                client.delete(&object.key).await?;
                pruned.push(object.key);
            }
            Ok::<_, anyhow::Error>(BackupResult {
                object_key: spec.object_key.clone(),
                size_bytes,
                pruned,
            })
        }
        .await;
        // Temporary archives never outlive the run.
        let _ = tokio::fs::remove_file(&out).await;
        result.map_err(internal)
    }
}
