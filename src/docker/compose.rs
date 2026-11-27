//! Docker Compose stacks via the host `docker compose` CLI (the Engine API has no
//! Compose support). Flow per deployment:
//!
//! 1. Policy check on the raw YAML ([`crate::validate::compose::check_raw`]).
//! 2. `docker compose config --format json` with ONLY the app's env vars → interpolated,
//!    normalized stack; policy check again on that output.
//! 3. Inject forgenerd settings (strip `ports`, `restart: unless-stopped`, labels).
//! 4. `docker compose up -d --remove-orphans --wait`, then delete the generated file
//!    (it holds interpolated secrets).
//! 5. Attach the exposed service to `forgenerd-internal` through the Docker API with the
//!    app name as its only alias. Declaring the network in the compose file instead would
//!    make Compose also register the *service* name there (e.g. `api`), so another stack's
//!    exposed container looking up its own private `api` could resolve this one.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{NETWORK, minimal_env, run_logged};
use crate::executor::{ComposeSpec, RunningContainer, compose_container_name, compose_project};
use crate::validate::compose as policy;

const CONFIG_TIMEOUT: Duration = Duration::from_secs(60);
const DOWN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn compose_cmd(project: &str, env: &std::collections::BTreeMap<String, String>) -> Command {
    let mut cmd = Command::new("docker");
    minimal_env(&mut cmd);
    cmd.envs(env).args(["compose", "-p", project]);
    cmd
}

/// Write a file readable only by forgenerd.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    std::io::Write::write_all(&mut file, contents)?;
    Ok(())
}

pub async fn deploy(
    docker: &Docker,
    compose_dir: &Path,
    spec: &ComposeSpec,
    log: &mpsc::Sender<String>,
) -> Result<Option<RunningContainer>> {
    let services = policy::check_raw(&spec.compose_yaml).map_err(|e| anyhow!("{e}"))?;
    if let Some(exposed) = &spec.expose_service
        && !services.contains(exposed)
    {
        bail!("compose_service `{exposed}` is not defined in the compose file");
    }

    let project = compose_project(&spec.app_name);
    let dir = compose_dir.join(&spec.app_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let source = dir.join("compose.yaml");
    let generated = dir.join("generated.json");
    write_private(&source, spec.compose_yaml.as_bytes())?;

    let result = async {
        let normalized = normalize(&project, &source, &spec.env).await?;
        policy::check_normalized(&normalized).map_err(|e| anyhow!("{e}"))?;
        let stack = inject(normalized, spec)?;
        write_private(&generated, serde_json::to_vec_pretty(&stack)?.as_slice())?;

        let _ = log
            .send(format!("starting compose project {project}"))
            .await;
        let mut up = compose_cmd(&project, &spec.env);
        up.arg("-f")
            .arg(&generated)
            .args(["up", "-d", "--remove-orphans", "--wait", "--wait-timeout"])
            .arg(spec.wait_timeout_secs.to_string());
        let timeout = Duration::from_secs(spec.wait_timeout_secs + 10 * 60);
        run_logged(up, log, timeout)
            .await
            .context("docker compose up failed")
    }
    .await;
    // The generated file contains interpolated secrets; never leave it on disk.
    let _ = std::fs::remove_file(&generated);
    result?;

    match &spec.expose_service {
        Some(service) => {
            attach(docker, &spec.app_name, service).await?;
            exposed_container(docker, &spec.app_name, service)
                .await
                .map(Some)
        }
        None => Ok(None),
    }
}

async fn normalize(
    project: &str,
    source: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Value> {
    let mut cmd = compose_cmd(project, env);
    cmd.arg("-f")
        .arg(source)
        .args(["config", "--format", "json"])
        .kill_on_drop(true);
    let output = tokio::time::timeout(CONFIG_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("docker compose config timed out"))??;
    if !output.status.success() {
        bail!(
            "invalid compose file: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing docker compose config output")
}

/// Apply forgenerd's settings to the normalized stack.
pub fn inject(mut stack: Value, spec: &ComposeSpec) -> Result<Value> {
    let services = stack
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("normalized stack has no services"))?;
    for (name, service) in services.iter_mut() {
        let service = service
            .as_object_mut()
            .ok_or_else(|| anyhow!("service {name} is not an object"))?;
        // Traffic only enters through Caddy; published ports would also bypass host
        // firewalls (Docker writes its own iptables rules).
        service.remove("ports");
        service.insert("restart".into(), json!("unless-stopped"));
        let labels = service
            .entry("labels")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("labels of {name} are not a mapping"))?;
        labels.insert("forgenerd.managed".into(), json!("true"));
        labels.insert("forgenerd.app_id".into(), json!(spec.app_id));
    }
    Ok(stack)
}

/// Connect the exposed service to `forgenerd-internal` (alias = app name) unless it is
/// already connected. Idempotent across redeploys.
async fn attach(docker: &Docker, app_name: &str, service: &str) -> Result<()> {
    let name = compose_container_name(app_name, service);
    let info = docker.inspect_container(&name, None).await?;
    let connected = info
        .network_settings
        .and_then(|s| s.networks)
        .is_some_and(|n| n.contains_key(NETWORK));
    if connected {
        return Ok(());
    }
    docker
        .connect_network(
            NETWORK,
            bollard::models::NetworkConnectRequest {
                container: name.clone(),
                endpoint_config: Some(bollard::models::EndpointSettings {
                    aliases: Some(vec![app_name.to_string()]),
                    ..Default::default()
                }),
            },
        )
        .await
        .with_context(|| format!("attaching {name} to {NETWORK}"))
}

async fn exposed_container(
    docker: &Docker,
    app_name: &str,
    service: &str,
) -> Result<RunningContainer> {
    let name = compose_container_name(app_name, service);
    let info = docker
        .inspect_container(&name, None)
        .await
        .with_context(|| format!("inspecting {name}"))?;
    let ip = info
        .network_settings
        .and_then(|s| s.networks)
        .and_then(|mut n| n.remove(NETWORK))
        .and_then(|e| e.ip_address)
        .filter(|ip| !ip.is_empty())
        .ok_or_else(|| anyhow!("{name} is not attached to {NETWORK}"))?;
    Ok(RunningContainer {
        id: info.id.unwrap_or_else(|| name.clone()),
        name,
        ip,
    })
}

/// `docker compose down` for a project (works without the compose file).
pub async fn down(project: &str, remove_volumes: bool) -> Result<()> {
    let mut cmd = compose_cmd(project, &Default::default());
    cmd.args(["down", "--remove-orphans"]);
    if remove_volumes {
        cmd.arg("--volumes");
    }
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            tracing::info!("{line}");
        }
    });
    let result = run_logged(cmd, &tx, DOWN_TIMEOUT).await;
    drop(tx);
    let _ = drain.await;
    result.context("docker compose down failed")
}

pub fn compose_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("compose")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(expose: Option<&str>) -> ComposeSpec {
        ComposeSpec {
            app_id: "app-1".into(),
            app_name: "shop".into(),
            deployment_id: "d".into(),
            compose_yaml: String::new(),
            env: Default::default(),
            expose_service: expose.map(String::from),
            wait_timeout_secs: 60,
        }
    }

    #[test]
    fn inject_strips_ports_and_never_declares_the_shared_network() {
        let stack = json!({
            "name": "forgenerd-shop",
            "services": {
                "web": {"image": "nginx", "ports": [{"target": 80, "published": "8080"}], "networks": {"default": null}},
                "db": {"image": "postgres", "restart": "no", "labels": {"tier": "data"}}
            }
        });
        let out = inject(stack, &spec(Some("web"))).unwrap();
        let web = &out["services"]["web"];
        assert!(web.get("ports").is_none());
        assert_eq!(web["restart"], "unless-stopped");
        assert_eq!(web["labels"]["forgenerd.app_id"], "app-1");
        assert!(
            web["networks"].get(NETWORK).is_none(),
            "attached via the API, not compose"
        );

        let db = &out["services"]["db"];
        assert_eq!(db["restart"], "unless-stopped");
        assert_eq!(db["labels"]["tier"], "data");
        assert!(out.get("networks").is_none());
    }
}
