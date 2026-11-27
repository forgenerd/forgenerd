//! Shared integration-test harness: an in-process router over a temp SQLite database,
//! with a fake `NodeExecutor` standing in for Docker and Caddy.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower::ServiceExt;

use forgenerd::api::{AppState, router};
use forgenerd::auth::crypto::Cipher;
use forgenerd::deploy::DeployTiming;
use forgenerd::error::AppError;
use forgenerd::executor::*;

pub const SETUP_TOKEN: &str = "test-setup-token";
pub const EMAIL: &str = "admin@example.com";
pub const PASSWORD: &str = "a-long-enough-password";

/// Records every call; health results are controllable.
#[derive(Default)]
pub struct FakeExecutor {
    pub unhealthy: AtomicBool,
    pub started: Mutex<Vec<ContainerSpec>>,
    pub removed: Mutex<Vec<String>>,
    pub removed_volumes: Mutex<Vec<String>>,
    pub actions: Mutex<Vec<(String, ContainerAction)>>,
    pub proxy: Mutex<Option<ProxyConfig>>,
    pub proxy_syncs: Mutex<usize>,
    pub builds: Mutex<Vec<BuildSpec>>,
    pub pruned: Mutex<Vec<(String, Vec<String>)>>,
    /// What `list_managed_containers` reports (the reconciler's view of Docker).
    pub containers: Mutex<Vec<ContainerHealth>>,
    pub composes: Mutex<Vec<ComposeSpec>>,
    pub aliases: Mutex<Vec<(String, String)>>,
    pub backups: Mutex<Vec<BackupSpec>>,
    pub backup_error: Mutex<Option<String>>,
    pub restores: Mutex<Vec<RestoreSpec>>,
    pub composes_removed: Mutex<Vec<(String, bool)>>,
    /// Makes `prepare_image` wait until notified (to observe queued deployments).
    pub hold_builds: tokio::sync::Mutex<()>,
}

impl FakeExecutor {
    pub fn set_unhealthy(&self, value: bool) {
        self.unhealthy.store(value, Ordering::SeqCst);
    }
    pub fn proxy(&self) -> ProxyConfig {
        self.proxy.lock().unwrap().clone().unwrap_or_default()
    }
    pub fn removed(&self) -> Vec<String> {
        self.removed.lock().unwrap().clone()
    }
    pub fn started(&self) -> Vec<ContainerSpec> {
        self.started.lock().unwrap().clone()
    }
}

pub fn fake_container_id(deployment_id: &str) -> String {
    format!("ctr-{deployment_id}")
}

#[async_trait]
impl NodeExecutor for FakeExecutor {
    async fn ensure_node_ready(&self) -> Result<(), AppError> {
        Ok(())
    }
    async fn prepare_image(
        &self,
        spec: &BuildSpec,
        log_tx: mpsc::Sender<String>,
    ) -> Result<PreparedImage, AppError> {
        let _hold = self.hold_builds.lock().await;
        self.builds.lock().unwrap().push(spec.clone());
        match spec {
            BuildSpec::Pull { image } => {
                let _ = log_tx.send(format!("fake pull {image}")).await;
                Ok(PreparedImage {
                    image_ref: image.clone(),
                    commit_sha: None,
                })
            }
            BuildSpec::Git {
                app_name,
                deployment_id,
                repo_url,
                ..
            } => {
                let _ = log_tx.send(format!("fake clone {repo_url}")).await;
                Ok(PreparedImage {
                    image_ref: format!("forgenerd/{app_name}:{deployment_id}"),
                    commit_sha: Some("abc123".into()),
                })
            }
        }
    }
    async fn start_container(&self, spec: &ContainerSpec) -> Result<RunningContainer, AppError> {
        self.started.lock().unwrap().push(spec.clone());
        Ok(RunningContainer {
            id: fake_container_id(&spec.deployment_id),
            name: container_name(&spec.app_name, &spec.deployment_id),
            ip: "10.0.0.2".into(),
        })
    }
    async fn wait_healthy(&self, _: &HealthTarget, _: Duration) -> Result<(), AppError> {
        if self.unhealthy.load(Ordering::SeqCst) {
            Err(AppError::Internal(anyhow::anyhow!("fake: not healthy")))
        } else {
            Ok(())
        }
    }
    async fn sync_proxy_routes(&self, config: &ProxyConfig) -> Result<(), AppError> {
        *self.proxy.lock().unwrap() = Some(config.clone());
        *self.proxy_syncs.lock().unwrap() += 1;
        Ok(())
    }
    async fn remove_container(&self, id: &str) -> Result<(), AppError> {
        self.removed.lock().unwrap().push(id.to_string());
        Ok(())
    }
    async fn publish_alias(&self, id: &str, alias: &str) -> Result<(), AppError> {
        self.aliases
            .lock()
            .unwrap()
            .push((id.to_string(), alias.to_string()));
        Ok(())
    }
    async fn probe_certificates(&self, hostnames: &[String]) -> Result<Vec<CertStatus>, AppError> {
        Ok(hostnames
            .iter()
            .map(|h| CertStatus {
                hostname: h.clone(),
                state: if h.starts_with("pending.") {
                    CertState::Pending
                } else {
                    CertState::Issued
                },
                issuer: Some("Let's Encrypt (R11)".into()),
                not_after: None,
                detail: None,
            })
            .collect())
    }
    async fn remove_volumes(&self, names: &[String]) -> Result<(), AppError> {
        self.removed_volumes
            .lock()
            .unwrap()
            .extend_from_slice(names);
        Ok(())
    }
    async fn set_container_state(&self, id: &str, action: ContainerAction) -> Result<(), AppError> {
        self.actions.lock().unwrap().push((id.to_string(), action));
        Ok(())
    }
    async fn deploy_compose(
        &self,
        spec: &ComposeSpec,
        log_tx: mpsc::Sender<String>,
    ) -> Result<Option<RunningContainer>, AppError> {
        let _ = log_tx
            .send(format!(
                "fake compose up {}",
                compose_project(&spec.app_name)
            ))
            .await;
        self.composes.lock().unwrap().push(spec.clone());
        Ok(spec
            .expose_service
            .as_ref()
            .map(|service| RunningContainer {
                id: format!("compose-{}-{service}", spec.app_id),
                name: compose_container_name(&spec.app_name, service),
                ip: "10.0.0.9".into(),
            }))
    }
    async fn remove_compose(&self, project: &str, remove_volumes: bool) -> Result<(), AppError> {
        self.composes_removed
            .lock()
            .unwrap()
            .push((project.to_string(), remove_volumes));
        Ok(())
    }
    async fn stream_logs(&self, _: &str) -> Result<LogStream, AppError> {
        Err(AppError::NotImplemented)
    }
    async fn list_managed_containers(&self) -> Result<Vec<ContainerHealth>, AppError> {
        Ok(self.containers.lock().unwrap().clone())
    }
    async fn prune_images(&self, app: &str, keep: &[String]) -> Result<Vec<String>, AppError> {
        self.pruned
            .lock()
            .unwrap()
            .push((app.to_string(), keep.to_vec()));
        Ok(vec![])
    }
    async fn prune_unused(&self) -> Result<u64, AppError> {
        Ok(0)
    }
    async fn run_restore(&self, spec: &RestoreSpec) -> Result<(), AppError> {
        self.restores.lock().unwrap().push(spec.clone());
        Ok(())
    }
    async fn container_stats(&self, ids: &[String]) -> Result<Vec<ContainerStats>, AppError> {
        Ok(ids
            .iter()
            .map(|id| ContainerStats {
                id: id.clone(),
                name: format!("container-{id}"),
                cpu_percent: 1.5,
                memory_bytes: 64 * 1024 * 1024,
                memory_limit_bytes: 512 * 1024 * 1024,
            })
            .collect())
    }
    async fn disk_usage(&self) -> Result<DiskUsage, AppError> {
        Ok(DiskUsage {
            images_bytes: 1000,
            filesystem_total_bytes: 10_000,
            filesystem_free_bytes: 4_000,
            ..Default::default()
        })
    }
    async fn run_backup(&self, spec: &BackupSpec) -> Result<BackupResult, AppError> {
        self.backups.lock().unwrap().push(spec.clone());
        if let Some(err) = self.backup_error.lock().unwrap().clone() {
            return Err(AppError::Internal(anyhow::anyhow!(err)));
        }
        Ok(BackupResult {
            object_key: spec.object_key.clone(),
            size_bytes: 1234,
            pruned: vec![],
        })
    }
}

pub struct TestApp {
    pub app: Router,
    pub state: AppState,
    pub fake: Arc<FakeExecutor>,
    _dir: tempfile::TempDir,
}

pub async fn test_app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let pool = forgenerd::db::connect(&dir.path().join("test.db"))
        .await
        .unwrap();
    let fake = Arc::new(FakeExecutor::default());
    let mut registry = NodeRegistry::default();
    registry.insert("local", fake.clone());
    let mut state = AppState::new(
        pool,
        Cipher::new(&[1u8; 32]),
        Some(SETUP_TOKEN.into()),
        registry,
        dir.path().to_path_buf(),
    );
    state.timing = DeployTiming {
        health_timeout: Duration::from_millis(100),
        drain: Duration::ZERO,
        heal_backoff: Duration::ZERO,
        heal_stable: Duration::ZERO,
    };
    TestApp {
        app: router(state.clone()),
        state,
        fake,
        _dir: dir,
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl TestApp {
    pub async fn send(&self, req: Request<Body>) -> Resp {
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    async fn with_body(&self, method: &str, path: &str, body: Value, auth: Option<&str>) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(auth) = auth {
            let (name, value) = auth_header(auth);
            req = req.header(name, value);
        }
        self.send(req.body(Body::from(body.to_string())).unwrap())
            .await
    }

    pub async fn post(&self, path: &str, body: Value, auth: Option<&str>) -> Resp {
        self.with_body("POST", path, body, auth).await
    }

    pub async fn patch(&self, path: &str, body: Value, auth: &str) -> Resp {
        self.with_body("PATCH", path, body, Some(auth)).await
    }

    pub async fn get(&self, path: &str, auth: Option<&str>) -> Resp {
        let mut req = Request::get(path);
        if let Some(auth) = auth {
            let (name, value) = auth_header(auth);
            req = req.header(name, value);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    pub async fn delete(&self, path: &str, auth: &str) -> Resp {
        let (name, value) = auth_header(auth);
        let req = Request::delete(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(name, value);
        self.send(req.body(Body::empty()).unwrap()).await
    }

    pub async fn setup(&self) {
        let res = self
            .post(
                "/api/v1/auth/setup",
                json!({"setup_token": SETUP_TOKEN, "email": EMAIL, "password": PASSWORD}),
                None,
            )
            .await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    /// Returns `cookie:<name=value>` usable as the `auth` argument.
    pub async fn login(&self) -> String {
        let res = self
            .post(
                "/api/v1/auth/login",
                json!({"email": EMAIL, "password": PASSWORD}),
                None,
            )
            .await;
        assert_eq!(res.status, StatusCode::OK);
        let set_cookie = res.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        format!("cookie:{}", set_cookie.split(';').next().unwrap())
    }

    /// Setup + login in one step.
    pub async fn admin(&self) -> String {
        self.setup().await;
        self.login().await
    }

    /// Ids of an app's deployments, oldest first.
    pub async fn deployment_ids(&self, app_id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM deployments WHERE app_id = ? ORDER BY started_at, id")
            .bind(app_id)
            .fetch_all(&self.state.db)
            .await
            .unwrap()
    }

    /// Poll a deployment until it leaves `queued`/`building`.
    pub async fn wait_deployment(&self, id: &str, auth: &str) -> Value {
        for _ in 0..200 {
            let dep = self
                .get(&format!("/api/v1/deployments/{id}"), Some(auth))
                .await
                .json();
            let status = dep["status"].as_str().unwrap_or_default().to_string();
            if status != "queued" && status != "building" {
                return dep;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("deployment {id} did not finish");
    }
}

/// `auth` is either `cookie:<pair>` or `bearer:<token>`.
pub fn auth_header(auth: &str) -> (header::HeaderName, String) {
    if let Some(pair) = auth.strip_prefix("cookie:") {
        (header::COOKIE, pair.to_string())
    } else if let Some(token) = auth.strip_prefix("bearer:") {
        (header::AUTHORIZATION, format!("Bearer {token}"))
    } else {
        panic!("bad auth spec")
    }
}
