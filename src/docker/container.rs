use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, HostConfig, Mount, MountType, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, RemoveVolumeOptionsBuilder,
    StopContainerOptionsBuilder,
};

use super::{NETWORK, is_not_found};
use crate::executor::{
    ContainerAction, ContainerSpec, ContainerStats, DiskUsage, HealthTarget, RunningContainer,
    container_name, volume_name,
};

const STOP_TIMEOUT_SECS: i32 = 10;

/// Create and start an app container on `forgenerd-internal` (no published ports).
pub async fn start(docker: &Docker, spec: &ContainerSpec) -> Result<RunningContainer> {
    let name = container_name(&spec.app_name, &spec.deployment_id);
    let labels = HashMap::from([
        ("forgenerd.managed".to_string(), "true".to_string()),
        ("forgenerd.app_id".to_string(), spec.app_id.clone()),
        (
            "forgenerd.deployment_id".to_string(),
            spec.deployment_id.clone(),
        ),
    ]);
    let mounts = spec
        .volumes
        .iter()
        .map(|v| Mount {
            target: Some(v.mount_path.clone()),
            source: Some(volume_name(&spec.app_name, &v.volume_name)),
            typ: Some(MountType::VOLUME),
            ..Default::default()
        })
        .collect();
    let body = ContainerCreateBody {
        image: Some(spec.image.clone()),
        env: Some(spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        labels: Some(labels),
        host_config: Some(HostConfig {
            network_mode: Some(NETWORK.into()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            mounts: Some(mounts),
            memory: spec.memory_limit_mb.map(|mb| i64::from(mb) * 1024 * 1024),
            // Same as the memory limit: no extra swap on top of it.
            memory_swap: spec.memory_limit_mb.map(|mb| i64::from(mb) * 1024 * 1024),
            nano_cpus: spec.cpu_limit.map(|cores| (cores * 1e9).round() as i64),
            ..Default::default()
        }),
        ..Default::default()
    };
    let options = CreateContainerOptionsBuilder::new().name(&name).build();
    let created = docker
        .create_container(Some(options), body)
        .await
        .with_context(|| format!("creating container {name}"))?;
    if let Err(err) = docker.start_container(&created.id, None).await {
        let _ = remove(docker, &created.id).await;
        return Err(err).with_context(|| format!("starting container {name}"));
    }
    let ip = container_ip(docker, &created.id).await?;
    Ok(RunningContainer {
        id: created.id,
        name,
        ip,
    })
}

async fn container_ip(docker: &Docker, id: &str) -> Result<String> {
    let info = docker.inspect_container(id, None).await?;
    info.network_settings
        .and_then(|s| s.networks)
        .and_then(|mut n| n.remove(NETWORK))
        .and_then(|e| e.ip_address)
        .filter(|ip| !ip.is_empty())
        .ok_or_else(|| anyhow!("container {id} has no address on {NETWORK}"))
}

/// Poll `http://<ip>:<port><path>` every second. Any HTTP response below 500 counts as
/// healthy (apps commonly answer 404 on `/`). Fails fast if the container stops running.
pub async fn wait_healthy(
    docker: &Docker,
    http: &reqwest::Client,
    target: &HealthTarget,
    timeout: Duration,
) -> Result<()> {
    let url = format!("http://{}:{}{}", target.host, target.port, target.path);
    let deadline = Instant::now() + timeout;
    let mut last_error: String;
    loop {
        let info = docker.inspect_container(&target.container_id, None).await?;
        let state = info.state.unwrap_or_default();
        if !state.running.unwrap_or(false) && !state.restarting.unwrap_or(false) {
            bail!(
                "container exited (exit code {})",
                state.exit_code.map_or("unknown".into(), |c| c.to_string())
            );
        }
        match http.get(&url).send().await {
            Ok(res) if res.status().as_u16() < 500 => return Ok(()),
            Ok(res) => last_error = format!("HTTP {}", res.status()),
            // Include the cause chain ("connection refused", ...), not just the top error.
            Err(err) => last_error = format!("{:#}", anyhow::Error::from(err)),
        }
        if Instant::now() >= deadline {
            bail!("not healthy after {}s: {last_error}", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// SIGTERM with a grace period, then remove. A missing container counts as removed.
pub async fn remove(docker: &Docker, id: &str) -> Result<()> {
    let stop = StopContainerOptionsBuilder::new()
        .t(STOP_TIMEOUT_SECS)
        .build();
    match docker.stop_container(id, Some(stop)).await {
        Ok(()) => {}
        Err(err) if is_not_found(&err) => return Ok(()),
        // 304 "already stopped" and other stop errors fall through to a forced remove.
        Err(_) => {}
    }
    let options = RemoveContainerOptionsBuilder::new().force(true).build();
    match docker.remove_container(id, Some(options)).await {
        Ok(()) => Ok(()),
        Err(err) if is_not_found(&err) => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing container {id}")),
    }
}

pub async fn set_state(docker: &Docker, id: &str, action: ContainerAction) -> Result<()> {
    let result = match action {
        ContainerAction::Start => docker.start_container(id, None).await,
        ContainerAction::Stop => {
            let stop = StopContainerOptionsBuilder::new()
                .t(STOP_TIMEOUT_SECS)
                .build();
            docker.stop_container(id, Some(stop)).await
        }
        ContainerAction::Restart => {
            let opts = bollard::query_parameters::RestartContainerOptionsBuilder::new()
                .t(STOP_TIMEOUT_SECS)
                .build();
            docker.restart_container(id, Some(opts)).await
        }
    };
    match result {
        Ok(()) => Ok(()),
        // 304: already in the requested state.
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 304, ..
        }) => Ok(()),
        Err(err) => Err(err).with_context(|| format!("{action:?} container {id}")),
    }
}

pub async fn remove_volumes(docker: &Docker, names: &[String]) -> Result<()> {
    for name in names {
        let options = RemoveVolumeOptionsBuilder::new().force(true).build();
        match docker.remove_volume(name, Some(options)).await {
            Ok(()) => {}
            Err(err) if is_not_found(&err) => {}
            Err(err) => return Err(err).with_context(|| format!("removing volume {name}")),
        }
    }
    Ok(())
}

/// Every container labelled `forgenerd.managed=true`, with restart/health state.
pub async fn list_managed(docker: &Docker) -> Result<Vec<crate::executor::ContainerHealth>> {
    let filters = HashMap::from([("label", vec!["forgenerd.managed=true"])]);
    let options = bollard::query_parameters::ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build();
    let summaries = docker
        .list_containers(Some(options))
        .await
        .context("listing containers")?;
    let mut out = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let Some(id) = summary.id else { continue };
        let name = summary
            .names
            .as_deref()
            .and_then(|names| names.first())
            .map(|name| name.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.clone());
        let labels = summary.labels.unwrap_or_default();
        let Some(app_id) = labels.get("forgenerd.app_id").cloned() else {
            continue;
        };
        // Compose containers carry no deployment id (see ContainerHealth).
        let deployment_id = labels
            .get("forgenerd.deployment_id")
            .cloned()
            .unwrap_or_default();
        // RestartCount and health are only available from inspect.
        let info = match docker.inspect_container(&id, None).await {
            Ok(info) => info,
            Err(err) if is_not_found(&err) => continue,
            Err(err) => return Err(err).context("inspecting container"),
        };
        let ip = info
            .network_settings
            .as_ref()
            .and_then(|s| s.networks.as_ref())
            .and_then(|n| n.get(NETWORK))
            .and_then(|e| e.ip_address.clone())
            .filter(|ip| !ip.is_empty());
        let state = info.state.unwrap_or_default();
        out.push(crate::executor::ContainerHealth {
            id,
            app_id,
            name,
            deployment_id,
            running: state.running.unwrap_or(false),
            restarting: state.restarting.unwrap_or(false),
            restart_count: info.restart_count.unwrap_or(0),
            health: state
                .health
                .and_then(|h| h.status)
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty() && s != "none"),
            ip,
        });
    }
    Ok(out)
}

/// Follow a container's stdout/stderr (last 200 lines first). The background task ends
/// when the receiver is dropped or the container stops.
pub fn stream_logs(docker: &Docker, id: &str) -> tokio::sync::mpsc::Receiver<String> {
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let options = bollard::query_parameters::LogsOptionsBuilder::new()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .tail("200")
        .build();
    let mut stream = docker.logs(id, Some(options));
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            let line = match item {
                Ok(output) => output.to_string(),
                Err(err) => format!("[log stream error: {err}]"),
            };
            if tx
                .send(line.trim_end_matches('\n').to_string())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    rx
}

/// Remove `forgenerd/<app_name>:*` images not in `keep`. Images still used by a container
/// are skipped by Docker (no force), so the running version is never affected.
pub async fn prune_images(docker: &Docker, app_name: &str, keep: &[String]) -> Result<Vec<String>> {
    let reference = format!("forgenerd/{app_name}");
    let filters = HashMap::from([("reference", vec![reference.as_str()])]);
    let options = bollard::query_parameters::ListImagesOptionsBuilder::new()
        .filters(&filters)
        .build();
    let images = docker
        .list_images(Some(options))
        .await
        .context("listing images")?;
    let mut removed = Vec::new();
    for image in images {
        for tag in image.repo_tags {
            if keep.contains(&tag) {
                continue;
            }
            let options = bollard::query_parameters::RemoveImageOptionsBuilder::new().build();
            match docker.remove_image(&tag, Some(options), None).await {
                Ok(_) => removed.push(tag),
                Err(err) if is_not_found(&err) => {}
                Err(err) => tracing::debug!(%tag, error = %err, "image not pruned"),
            }
        }
    }
    Ok(removed)
}

/// Remove dangling (untagged) images and build cache no image uses; returns bytes freed.
pub async fn prune_unused(docker: &Docker) -> Result<u64> {
    let filters = HashMap::from([("dangling".to_string(), vec!["true".to_string()])]);
    let images = docker
        .prune_images(Some(bollard::query_parameters::PruneImagesOptions {
            filters: Some(filters),
        }))
        .await
        .context("pruning dangling images")?;
    let build = docker
        .prune_build(None::<bollard::query_parameters::PruneBuildOptions>)
        .await
        .context("pruning build cache")?;
    let bytes = images.space_reclaimed.unwrap_or(0) + build.space_reclaimed.unwrap_or(0);
    Ok(u64::try_from(bytes).unwrap_or(0))
}

/// One stats sample per container. Docker waits for a second sample internally, so the
/// CPU figure covers about one second. Containers that are gone are skipped.
pub async fn stats(docker: &Docker, ids: &[String]) -> Result<Vec<ContainerStats>> {
    use futures_util::StreamExt;

    let samples = ids.iter().map(|id| async move {
        let options = bollard::query_parameters::StatsOptionsBuilder::new()
            .stream(false)
            .build();
        let sample = docker.stats(id, Some(options)).next().await?;
        match sample {
            Ok(sample) => Some(Ok(to_stats(id, sample))),
            Err(err) if is_not_found(&err) => None,
            Err(err) => Some(Err(anyhow!(err).context(format!("reading stats of {id}")))),
        }
    });
    futures_util::future::join_all(samples)
        .await
        .into_iter()
        .flatten()
        .collect()
}

fn to_stats(id: &str, s: bollard::models::ContainerStatsResponse) -> ContainerStats {
    let total = |c: &Option<bollard::models::ContainerCpuStats>| {
        let c = c.as_ref();
        (
            c.and_then(|c| c.cpu_usage.as_ref())
                .and_then(|u| u.total_usage)
                .unwrap_or(0),
            c.and_then(|c| c.system_cpu_usage).unwrap_or(0),
        )
    };
    let (cpu, system) = total(&s.cpu_stats);
    let (pre_cpu, pre_system) = total(&s.precpu_stats);
    let cpus = s
        .cpu_stats
        .as_ref()
        .and_then(|c| c.online_cpus)
        .unwrap_or(1)
        .max(1);
    let cpu_delta = cpu.saturating_sub(pre_cpu) as f64;
    let system_delta = system.saturating_sub(pre_system) as f64;
    let cpu_percent = if system_delta > 0.0 {
        cpu_delta / system_delta * f64::from(cpus) * 100.0
    } else {
        0.0
    };
    let memory = s.memory_stats.unwrap_or_default();
    // Like `docker stats`: page cache that can be reclaimed is not counted as used.
    let cache = memory
        .stats
        .as_ref()
        .and_then(|st| {
            st.get("inactive_file")
                .or_else(|| st.get("total_inactive_file"))
        })
        .copied()
        .unwrap_or(0);
    ContainerStats {
        id: id.to_string(),
        name: s
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string(),
        cpu_percent: (cpu_percent * 10.0).round() / 10.0,
        memory_bytes: memory.usage.unwrap_or(0).saturating_sub(cache),
        memory_limit_bytes: memory.limit.unwrap_or(0),
    }
}

/// Docker's disk usage by kind, plus total and free space of the filesystem at `path`.
pub async fn disk_usage(docker: &Docker, path: &std::path::Path) -> Result<DiskUsage> {
    let df = docker
        .df(None::<bollard::query_parameters::DataUsageOptions>)
        .await
        .context("reading Docker disk usage")?;
    let size = |v: Option<i64>| u64::try_from(v.unwrap_or(0)).unwrap_or(0);
    let (filesystem_total_bytes, filesystem_free_bytes) = filesystem_space(path)?;
    Ok(DiskUsage {
        images_bytes: size(df.image_usage.as_ref().and_then(|u| u.total_size)),
        images_reclaimable_bytes: size(df.image_usage.as_ref().and_then(|u| u.reclaimable)),
        containers_bytes: size(df.container_usage.as_ref().and_then(|u| u.total_size)),
        volumes_bytes: size(df.volume_usage.as_ref().and_then(|u| u.total_size)),
        build_cache_bytes: size(df.build_cache_usage.as_ref().and_then(|u| u.total_size)),
        filesystem_total_bytes,
        filesystem_free_bytes,
    })
}

/// (total, available to unprivileged users) bytes of the filesystem holding `path`.
fn filesystem_space(path: &std::path::Path) -> Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a writable
    // statvfs buffer; it is only read after statvfs reports success.
    let stat = unsafe {
        if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("statvfs {}", path.display()));
        }
        stat.assume_init()
    };
    let block = stat.f_frsize as u64;
    Ok((stat.f_blocks as u64 * block, stat.f_bavail as u64 * block))
}

/// Disconnect and reconnect `forgenerd-internal` with a network alias. Endpoint aliases
/// cannot be changed in place; the brief interruption happens before traffic is routed.
pub async fn publish_alias(docker: &Docker, id: &str, alias: &str) -> Result<()> {
    docker
        .disconnect_network(
            NETWORK,
            bollard::models::NetworkDisconnectRequest {
                container: id.to_string(),
                force: Some(true),
            },
        )
        .await
        .with_context(|| format!("detaching {id} from {NETWORK}"))?;
    docker
        .connect_network(
            NETWORK,
            bollard::models::NetworkConnectRequest {
                container: id.to_string(),
                endpoint_config: Some(bollard::models::EndpointSettings {
                    aliases: Some(vec![alias.to_string()]),
                    ..Default::default()
                }),
            },
        )
        .await
        .with_context(|| format!("attaching {id} to {NETWORK} as {alias}"))
}
