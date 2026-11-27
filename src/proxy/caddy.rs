//! Caddy runs as the `forgenerd-caddy` container on `forgenerd-internal`. Its admin API
//! listens only on a Unix socket in a host directory bind-mounted into the container, so
//! neither the host network nor app containers can reach it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, HostConfig, Mount, MountType, PortBinding,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::CreateContainerOptionsBuilder;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

use crate::docker::{self, NETWORK, is_not_found};
use crate::executor::ProxyConfig;

pub const CONTAINER: &str = "forgenerd-caddy";
pub const IMAGE: &str = "caddy:2.10.2-alpine";
/// Mount point of the host's `<data_dir>/caddy` directory inside the Caddy container.
const RUN_DIR: &str = "/run/forgenerd";
const SOCKET_NAME: &str = "admin.sock";
/// Linux `sun_path` limit (108 bytes including the NUL terminator).
const MAX_SOCKET_PATH: usize = 107;

/// Host-side paths for the Caddy admin socket.
#[derive(Debug, Clone)]
pub struct CaddyPaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
}

impl CaddyPaths {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("caddy");
        let socket = dir.join(SOCKET_NAME);
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            bail!(
                "Caddy admin socket path is too long ({} > {MAX_SOCKET_PATH} bytes): {}; use a shorter FORGENERD_DATA_DIR",
                socket.as_os_str().len(),
                socket.display()
            );
        }
        Ok(Self { dir, socket })
    }
}

fn admin_listen() -> String {
    // 0666 on the socket; access is restricted by the 0700 directory that contains it.
    format!("unix/{RUN_DIR}/{SOCKET_NAME}|0666")
}

/// Ensure the Caddy container exists and is running.
pub async fn ensure_container(docker: &Docker, paths: &CaddyPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;
    std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o700))?;
    // Used only when Caddy has no autosaved config yet (`--resume` takes precedence).
    let bootstrap = json!({ "admin": { "listen": admin_listen() } });
    std::fs::write(paths.dir.join("bootstrap.json"), bootstrap.to_string())?;

    let host_dir = paths
        .dir
        .to_str()
        .ok_or_else(|| anyhow!("data dir path is not valid UTF-8"))?
        .to_string();

    match docker.inspect_container(CONTAINER, None).await {
        Ok(info) => {
            let image = info.config.as_ref().and_then(|c| c.image.as_deref());
            let mounted_here = info.mounts.iter().flatten().any(|m| {
                m.destination.as_deref() == Some(RUN_DIR) && m.source.as_deref() == Some(&host_dir)
            });
            if image == Some(IMAGE) && mounted_here {
                let running =
                    info.state.and_then(|s| s.status) == Some(ContainerStateStatusEnum::RUNNING);
                if !running {
                    tracing::info!(container = CONTAINER, "starting stopped proxy container");
                    docker
                        .start_container(CONTAINER, None)
                        .await
                        .context("starting Caddy")?;
                }
                return Ok(());
            }
            // Different data dir or Caddy version: recreate. Certificates live in the
            // `forgenerd-caddy-data` volume and survive.
            tracing::info!(
                container = CONTAINER,
                "proxy container config changed (image or data dir); recreating"
            );
            crate::docker::container::remove(docker, CONTAINER).await?;
        }
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(err).context("inspecting Caddy container"),
    }

    tracing::info!(image = IMAGE, "creating proxy container");
    if docker.inspect_image(IMAGE).await.is_err() {
        docker::pull_image(docker, IMAGE, &mut |line| tracing::info!("{line}")).await?;
    }
    let bind = |port: &str| {
        (
            port.to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".into()),
                host_port: Some(port.split('/').next().unwrap_or(port).into()),
            }]),
        )
    };
    let volume = |name: &str, target: &str| Mount {
        source: Some(name.into()),
        target: Some(target.into()),
        typ: Some(MountType::VOLUME),
        ..Default::default()
    };
    let body = ContainerCreateBody {
        image: Some(IMAGE.into()),
        cmd: Some(
            [
                "caddy",
                "run",
                "--resume",
                "--config",
                &format!("{RUN_DIR}/bootstrap.json"),
            ]
            .map(String::from)
            .to_vec(),
        ),
        labels: Some(HashMap::from([("forgenerd.role".into(), "proxy".into())])),
        exposed_ports: Some(vec!["80/tcp".into(), "443/tcp".into(), "443/udp".into()]),
        host_config: Some(HostConfig {
            network_mode: Some(NETWORK.into()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            port_bindings: Some(HashMap::from([
                bind("80/tcp"),
                bind("443/tcp"),
                bind("443/udp"),
            ])),
            mounts: Some(vec![
                Mount {
                    source: Some(host_dir),
                    target: Some(RUN_DIR.into()),
                    typ: Some(MountType::BIND),
                    ..Default::default()
                },
                volume("forgenerd-caddy-data", "/data"),
                volume("forgenerd-caddy-config", "/config"),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let options = CreateContainerOptionsBuilder::new().name(CONTAINER).build();
    docker
        .create_container(Some(options), body)
        .await
        .context("creating Caddy container")?;
    docker
        .start_container(CONTAINER, None)
        .await
        .context("starting Caddy container (are ports 80/443 free?)")?;
    Ok(())
}

/// Build the COMPLETE Caddy JSON config. `panel_upstream` is where the panel hostname
/// (if any) is proxied to.
pub fn build_config(config: &ProxyConfig, panel_upstream: Option<SocketAddr>) -> Value {
    let proxy_route = |hosts: &[String], dial: &str| {
        json!({
            "match": [{ "host": hosts }],
            "handle": [{ "handler": "reverse_proxy", "upstreams": [{ "dial": dial }] }],
            "terminal": true
        })
    };

    let mut https_routes: Vec<Value> = Vec::new();
    let mut http_routes: Vec<Value> = Vec::new();
    if let (Some(host), Some(upstream)) = (&config.panel_hostname, panel_upstream) {
        https_routes.push(proxy_route(
            std::slice::from_ref(host),
            &upstream.to_string(),
        ));
    }
    for route in &config.routes {
        https_routes.push(proxy_route(&route.hostnames, &route.upstream));
        if !route.force_https {
            http_routes.push(proxy_route(&route.hostnames, &route.upstream));
        }
    }

    let mut servers = serde_json::Map::new();
    servers.insert(
        "https".into(),
        json!({ "listen": [":443"], "routes": https_routes }),
    );
    // Hosts that opted out of HTTPS redirects are served on :80 explicitly. Caddy appends
    // its automatic HTTP->HTTPS redirects for every other host to this server.
    if !http_routes.is_empty() {
        servers.insert(
            "http".into(),
            json!({ "listen": [":80"], "routes": http_routes }),
        );
    }

    let mut apps = json!({ "http": { "servers": servers } });
    if let Some(email) = &config.acme_email {
        apps["tls"] = json!({
            "automation": { "policies": [{
                "issuers": [{ "module": "acme", "email": email }]
            }]}
        });
    }
    json!({ "admin": { "listen": admin_listen() }, "apps": apps })
}

/// POST the full config to the admin API's `/load` endpoint over the Unix socket.
pub async fn load(socket: &Path, config: &Value) -> Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to Caddy admin socket {}", socket.display()))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("Caddy admin handshake")?;
    tokio::spawn(async move {
        if let Err(err) = conn.await {
            tracing::debug!(error = %err, "Caddy admin connection closed");
        }
    });
    let request = hyper::Request::post("/load")
        .header(hyper::header::HOST, "localhost")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(config.to_string())))?;
    let response = sender
        .send_request(request)
        .await
        .context("Caddy /load request")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.into_body().collect().await?.to_bytes();
        bail!(
            "Caddy rejected config ({status}): {}",
            String::from_utf8_lossy(&body).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ProxyRoute;

    fn route(host: &str, upstream: &str, force_https: bool) -> ProxyRoute {
        ProxyRoute {
            hostnames: vec![host.into()],
            upstream: upstream.into(),
            force_https,
        }
    }

    #[test]
    fn empty_config_keeps_admin_socket() {
        let cfg = build_config(&ProxyConfig::default(), None);
        assert_eq!(
            cfg["admin"]["listen"],
            "unix//run/forgenerd/admin.sock|0666"
        );
        assert_eq!(cfg["apps"]["http"]["servers"]["https"]["routes"], json!([]));
        assert!(cfg["apps"]["http"]["servers"].get("http").is_none());
        assert!(cfg["apps"].get("tls").is_none());
    }

    #[test]
    fn routes_panel_and_http_opt_out() {
        let cfg = build_config(
            &ProxyConfig {
                routes: vec![
                    route("a.example.com", "forgenerd-a-1:8080", true),
                    route("b.example.com", "forgenerd-b-2:3000", false),
                ],
                panel_hostname: Some("panel.example.com".into()),
                acme_email: Some("ops@example.com".into()),
            },
            Some("172.30.0.1:4367".parse().unwrap()),
        );
        let https = &cfg["apps"]["http"]["servers"]["https"]["routes"];
        assert_eq!(https.as_array().unwrap().len(), 3);
        assert_eq!(https[0]["match"][0]["host"][0], "panel.example.com");
        assert_eq!(
            https[0]["handle"][0]["upstreams"][0]["dial"],
            "172.30.0.1:4367"
        );
        assert_eq!(
            https[2]["handle"][0]["upstreams"][0]["dial"],
            "forgenerd-b-2:3000"
        );
        let http = &cfg["apps"]["http"]["servers"]["http"]["routes"];
        assert_eq!(http.as_array().unwrap().len(), 1);
        assert_eq!(http[0]["match"][0]["host"][0], "b.example.com");
        assert_eq!(
            cfg["apps"]["tls"]["automation"]["policies"][0]["issuers"][0]["email"],
            "ops@example.com"
        );
    }

    #[test]
    fn panel_needs_upstream() {
        let cfg = build_config(
            &ProxyConfig {
                panel_hostname: Some("panel.example.com".into()),
                ..Default::default()
            },
            None,
        );
        assert_eq!(cfg["apps"]["http"]["servers"]["https"]["routes"], json!([]));
    }

    #[test]
    fn rejects_long_socket_paths() {
        assert!(CaddyPaths::new(Path::new("/var/lib/forgenerd")).is_ok());
        assert!(CaddyPaths::new(&PathBuf::from(format!("/{}", "x".repeat(120)))).is_err());
    }
}
