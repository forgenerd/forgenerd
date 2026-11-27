//! Thin wrapper around the local Docker Engine (bollard). Only `executor::local` uses it.

pub mod backup;
pub mod builder;
pub mod compose;
pub mod container;

use std::collections::HashMap;
use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::NetworkCreateRequest;
use futures_util::StreamExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

pub const NETWORK: &str = "forgenerd-internal";

pub fn connect() -> Result<Docker> {
    Docker::connect_with_local_defaults().context("connecting to the Docker socket")
}

pub fn is_not_found(err: &DockerError) -> bool {
    matches!(
        err,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

/// Ensure the `forgenerd-internal` bridge network exists and return its gateway address
/// (the host's IP on that network).
pub async fn ensure_network(docker: &Docker) -> Result<IpAddr> {
    match docker.inspect_network(NETWORK, None).await {
        Ok(_) => {}
        Err(err) if is_not_found(&err) => {
            tracing::info!(network = NETWORK, "creating Docker network");
            docker
                .create_network(NetworkCreateRequest {
                    name: NETWORK.into(),
                    driver: Some("bridge".into()),
                    labels: Some(HashMap::from([("forgenerd.managed".into(), "true".into())])),
                    ..Default::default()
                })
                .await
                .with_context(|| format!("creating network {NETWORK}"))?;
        }
        Err(err) => return Err(err).with_context(|| format!("inspecting network {NETWORK}")),
    }

    let network = docker
        .inspect_network(NETWORK, None)
        .await
        .with_context(|| format!("inspecting network {NETWORK}"))?;
    network
        .ipam
        .and_then(|ipam| ipam.config)
        .into_iter()
        .flatten()
        .filter_map(|cfg| cfg.gateway)
        .find_map(|gw| gw.parse::<IpAddr>().ok().filter(IpAddr::is_ipv4))
        .ok_or_else(|| anyhow!("network {NETWORK} has no IPv4 gateway"))
}

/// Pull `image` unless it is already present, forwarding progress lines to `log`.
pub async fn pull_image(docker: &Docker, image: &str, log: &mut impl FnMut(String)) -> Result<()> {
    let (from_image, tag) = split_tag(image);
    let options = bollard::query_parameters::CreateImageOptionsBuilder::new()
        .from_image(from_image)
        .tag(tag)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    let mut last = String::new();
    while let Some(item) = stream.next().await {
        let info = item.with_context(|| format!("pulling {image}"))?;
        if let Some(err) = info.error_detail.and_then(|d| d.message) {
            return Err(anyhow!("pulling {image}: {err}"));
        }
        // Skip per-chunk progress bars; keep one line per layer state change.
        if info
            .progress_detail
            .as_ref()
            .is_some_and(|p| p.current.is_some())
        {
            continue;
        }
        let line = match (info.id, info.status) {
            (Some(id), Some(status)) => format!("{id}: {status}"),
            (None, Some(status)) => status,
            _ => continue,
        };
        if line != last {
            log(line.clone());
            last = line;
        }
    }
    Ok(())
}

/// Split `repo[:tag][@digest]` into the `fromImage` / `tag` pair the API expects.
fn split_tag(image: &str) -> (&str, &str) {
    if image.contains('@') {
        return (image, "");
    }
    let last_segment = image.rsplit('/').next().unwrap_or(image);
    match last_segment.rsplit_once(':') {
        Some((_, tag)) => (&image[..image.len() - tag.len() - 1], tag),
        None => (image, "latest"),
    }
}

/// Environment for child processes that must not inherit forgenerd's own environment
/// (which may hold `FORGENERD_MASTER_KEY`): only what the Docker CLI needs.
pub(crate) fn minimal_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in [
        "PATH",
        "HOME",
        "DOCKER_HOST",
        "DOCKER_CONFIG",
        "DOCKER_CONTEXT",
        "DOCKER_CERT_PATH",
        "DOCKER_TLS_VERIFY",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

/// Run a command, forwarding stdout and stderr lines to `log`; fail on non-zero exit or
/// timeout (the child is killed when dropped).
pub(crate) async fn run_logged(
    mut cmd: Command,
    log: &mpsc::Sender<String>,
    timeout: Duration,
) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

    let pump = async {
        let mut out = BufReader::new(stdout).lines();
        let mut err = BufReader::new(stderr).lines();
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = out.next_line(), if !out_done => match line? {
                    Some(l) => if !l.trim().is_empty() { let _ = log.send(l).await; },
                    None => out_done = true,
                },
                line = err.next_line(), if !err_done => match line? {
                    Some(l) => if !l.trim().is_empty() { let _ = log.send(l).await; },
                    None => err_done = true,
                },
            }
        }
        child.wait().await
    };
    let status = tokio::time::timeout(timeout, pump)
        .await
        .map_err(|_| anyhow!("timed out after {}s", timeout.as_secs()))??;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn run_logged_captures_both_streams_and_exit_code() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let err = run_logged(cmd, &tx, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exit status: 3"), "{err}");
        drop(tx);
        let mut lines = Vec::new();
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        lines.sort();
        assert_eq!(lines, vec!["err", "out"]);
    }

    #[tokio::test]
    async fn run_logged_times_out() {
        let (tx, _rx) = mpsc::channel(16);
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let err = run_logged(cmd, &tx, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn splits_tags() {
        assert_eq!(split_tag("nginx"), ("nginx", "latest"));
        assert_eq!(split_tag("nginx:1.27"), ("nginx", "1.27"));
        assert_eq!(split_tag("ghcr.io:443/a/b:v1"), ("ghcr.io:443/a/b", "v1"));
        assert_eq!(
            split_tag("localhost:5000/app"),
            ("localhost:5000/app", "latest")
        );
        assert_eq!(split_tag("a/b@sha256:abc"), ("a/b@sha256:abc", ""));
    }
}
