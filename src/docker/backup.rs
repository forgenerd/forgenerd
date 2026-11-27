//! Produce and restore backup archives on the node: database dumps via `docker exec`
//! inside the database container, and named-volume archives via the Engine's archive
//! API. Everything streams through 0600 temp files; nothing is buffered in memory.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::StartExecResults;
use bollard::models::{ContainerCreateBody, ExecConfig, HostConfig, Mount, MountType};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, DownloadFromContainerOptionsBuilder,
};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::executor::BackupSource;

/// Maximum stderr kept for error messages.
const STDERR_LIMIT: usize = 4096;

/// Shell scripts take user-provided values ONLY from environment variables (`FN_*`), never
/// interpolated into the script text, so database names or passwords cannot inject
/// commands. Defaults fall back to the container's own environment.
const PG_DUMP: &str = r#"export PGPASSWORD="${FN_PASSWORD:-${POSTGRES_PASSWORD:-}}"; exec pg_dump --format=custom -U "${FN_USER:-${POSTGRES_USER:-postgres}}" -d "${FN_DB:-${POSTGRES_DB:-${FN_USER:-${POSTGRES_USER:-postgres}}}}""#;
const MYSQL_DUMP: &str = r#"export MYSQL_PWD="${FN_PASSWORD:-${MYSQL_ROOT_PASSWORD:-}}"; exec mysqldump --single-transaction --routines --triggers -u"${FN_USER:-root}" "${FN_DB:-${MYSQL_DATABASE}}""#;
/// `--clean --if-exists` drops the objects in the dump before recreating them, so the
/// database ends up exactly as it was when the dump was taken.
const PG_RESTORE: &str = r#"export PGPASSWORD="${FN_PASSWORD:-${POSTGRES_PASSWORD:-}}"; exec pg_restore --clean --if-exists --no-owner --single-transaction -U "${FN_USER:-${POSTGRES_USER:-postgres}}" -d "${FN_DB:-${POSTGRES_DB:-${FN_USER:-${POSTGRES_USER:-postgres}}}}""#;
/// mysqldump output already starts each table with `DROP TABLE IF EXISTS`.
const MYSQL_RESTORE: &str = r#"export MYSQL_PWD="${FN_PASSWORD:-${MYSQL_ROOT_PASSWORD:-}}"; exec mysql -u"${FN_USER:-root}" "${FN_DB:-${MYSQL_DATABASE}}""#;

fn private_file(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// Create the backup archive for `source` at `out`. Returns the archive path.
pub async fn create(
    docker: &Docker,
    source: &BackupSource,
    helper_image: &str,
    out: &Path,
) -> Result<PathBuf> {
    match source {
        BackupSource::Postgres {
            container,
            db_name,
            db_user,
            db_password,
        } => {
            let env = credentials_env(db_name, db_user, db_password);
            exec_to_file(docker, container, PG_DUMP, env, out).await?;
            Ok(out.to_path_buf())
        }
        BackupSource::Mysql {
            container,
            db_name,
            db_user,
            db_password,
        } => {
            let raw = out.with_extension("raw");
            let env = credentials_env(db_name, db_user, db_password);
            let result = async {
                exec_to_file(docker, container, MYSQL_DUMP, env, &raw).await?;
                gzip(&raw, out).await
            }
            .await;
            let _ = tokio::fs::remove_file(&raw).await;
            result.map(|()| out.to_path_buf())
        }
        BackupSource::Volume { volume_name } => {
            let raw = out.with_extension("raw");
            let result = async {
                volume_tar(docker, volume_name, helper_image, &raw).await?;
                gzip(&raw, out).await
            }
            .await;
            let _ = tokio::fs::remove_file(&raw).await;
            result.map(|()| out.to_path_buf())
        }
    }
}

/// Restore `archive` (as produced by [`create`] for the same kind of source) into `target`.
/// Database restores replace the dumped objects; volume restores replace the volume's
/// whole contents, so the app using the volume should be stopped first.
pub async fn restore(
    docker: &Docker,
    target: &BackupSource,
    helper_image: &str,
    archive: &Path,
) -> Result<()> {
    match target {
        BackupSource::Postgres {
            container,
            db_name,
            db_user,
            db_password,
        } => {
            let env = credentials_env(db_name, db_user, db_password);
            exec_from_file(docker, container, PG_RESTORE, env, archive).await
        }
        BackupSource::Mysql {
            container,
            db_name,
            db_user,
            db_password,
        } => {
            let raw = archive.with_extension("raw");
            let env = credentials_env(db_name, db_user, db_password);
            let result = async {
                gunzip(archive, &raw).await?;
                exec_from_file(docker, container, MYSQL_RESTORE, env, &raw).await
            }
            .await;
            let _ = tokio::fs::remove_file(&raw).await;
            result
        }
        BackupSource::Volume { volume_name } => {
            let raw = archive.with_extension("raw");
            let result = async {
                gunzip(archive, &raw).await?;
                volume_restore(docker, volume_name, helper_image, &raw).await
            }
            .await;
            let _ = tokio::fs::remove_file(&raw).await;
            result
        }
    }
}

fn credentials_env(
    db: &Option<String>,
    user: &Option<String>,
    password: &Option<String>,
) -> Vec<String> {
    [("FN_DB", db), ("FN_USER", user), ("FN_PASSWORD", password)]
        .into_iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}")))
        .collect()
}

/// Run `sh -c <script>` in a container, streaming stdout into `out`.
async fn exec_to_file(
    docker: &Docker,
    container: &str,
    script: &str,
    env: Vec<String>,
    out: &Path,
) -> Result<()> {
    let exec = docker
        .create_exec(
            container,
            ExecConfig {
                cmd: Some(vec!["sh".into(), "-c".into(), script.into()]),
                env: Some(env),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("starting dump in {container}"))?;
    let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await?
    else {
        bail!("dump exec unexpectedly detached");
    };
    let mut file = tokio::fs::File::from_std(private_file(out)?);
    let mut stderr = Vec::new();
    while let Some(chunk) = output.next().await {
        match chunk? {
            LogOutput::StdOut { message } => file.write_all(&message).await?,
            LogOutput::StdErr { message } if stderr.len() < STDERR_LIMIT => {
                stderr.extend_from_slice(&message);
            }
            _ => {}
        }
    }
    file.flush().await?;
    let code = docker.inspect_exec(&exec.id).await?.exit_code.unwrap_or(-1);
    if code != 0 {
        bail!(
            "dump exited with code {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(())
}

/// Run `sh -c <script>` in a container with `input` on stdin.
async fn exec_from_file(
    docker: &Docker,
    container: &str,
    script: &str,
    env: Vec<String>,
    input: &Path,
) -> Result<()> {
    let exec = docker
        .create_exec(
            container,
            ExecConfig {
                cmd: Some(vec!["sh".into(), "-c".into(), script.into()]),
                env: Some(env),
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("starting restore in {container}"))?;
    let StartExecResults::Attached {
        mut output,
        input: mut stdin,
    } = docker.start_exec(&exec.id, None).await?
    else {
        bail!("restore exec unexpectedly detached");
    };
    let mut file = tokio::fs::File::open(input)
        .await
        .with_context(|| format!("opening {}", input.display()))?;
    let writer = tokio::spawn(async move {
        tokio::io::copy(&mut file, &mut stdin).await?;
        stdin.shutdown().await
    });
    let mut stderr = Vec::new();
    while let Some(chunk) = output.next().await {
        match chunk? {
            LogOutput::StdErr { message } | LogOutput::StdOut { message }
                if stderr.len() < STDERR_LIMIT =>
            {
                stderr.extend_from_slice(&message);
            }
            _ => {}
        }
    }
    let written = writer.await.map_err(|e| anyhow!(e))?;
    let code = docker.inspect_exec(&exec.id).await?.exit_code.unwrap_or(-1);
    if code != 0 {
        bail!(
            "restore exited with code {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    written.context("sending the backup to the restore process")?;
    Ok(())
}

/// Tar a named volume through a never-started helper container that mounts it read-only.
async fn volume_tar(docker: &Docker, volume: &str, helper_image: &str, out: &Path) -> Result<()> {
    let name = format!("forgenerd-backup-{}", uuid::Uuid::now_v7().simple());
    let body = ContainerCreateBody {
        image: Some(helper_image.into()),
        cmd: Some(vec!["true".into()]),
        labels: Some(HashMap::from([("forgenerd.role".into(), "backup".into())])),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()),
            mounts: Some(vec![Mount {
                source: Some(volume.into()),
                target: Some("/data".into()),
                typ: Some(MountType::VOLUME),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    docker
        .create_container(
            Some(CreateContainerOptionsBuilder::new().name(&name).build()),
            body,
        )
        .await
        .with_context(|| format!("creating backup helper for volume {volume}"))?;
    let result = async {
        let options = DownloadFromContainerOptionsBuilder::new()
            .path("/data")
            .build();
        let mut stream = docker.download_from_container(&name, Some(options));
        let mut file = tokio::fs::File::from_std(private_file(out)?);
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk.context("reading volume archive")?)
                .await?;
        }
        file.flush().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let options = bollard::query_parameters::RemoveContainerOptionsBuilder::new()
        .force(true)
        .build();
    let _ = docker.remove_container(&name, Some(options)).await;
    result
}

/// Empty a named volume and unpack `tar` (an archive of `/data`, as [`volume_tar`]
/// produces) into it, through a helper container that mounts it read-write.
async fn volume_restore(
    docker: &Docker,
    volume: &str,
    helper_image: &str,
    tar: &Path,
) -> Result<()> {
    let name = format!("forgenerd-restore-{}", uuid::Uuid::now_v7().simple());
    let body = ContainerCreateBody {
        image: Some(helper_image.into()),
        cmd: Some(vec![
            "find".into(),
            "/data".into(),
            "-mindepth".into(),
            "1".into(),
            "-delete".into(),
        ]),
        labels: Some(HashMap::from([("forgenerd.role".into(), "restore".into())])),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()),
            mounts: Some(vec![Mount {
                source: Some(volume.into()),
                target: Some("/data".into()),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    docker
        .create_container(
            Some(CreateContainerOptionsBuilder::new().name(&name).build()),
            body,
        )
        .await
        .with_context(|| format!("creating restore helper for volume {volume}"))?;
    let result = async {
        docker.start_container(&name, None).await?;
        let mut wait = docker.wait_container(&name, None);
        while let Some(status) = wait.next().await {
            let status = status.context("emptying the volume")?;
            if status.status_code != 0 {
                bail!(
                    "emptying the volume exited with code {}",
                    status.status_code
                );
            }
        }
        let file = tokio::fs::File::open(tar)
            .await
            .with_context(|| format!("opening {}", tar.display()))?;
        let options = bollard::query_parameters::UploadToContainerOptionsBuilder::new()
            .path("/")
            .build();
        docker
            .upload_to_container(
                &name,
                Some(options),
                bollard::body_try_stream(tokio_util::io::ReaderStream::new(file)),
            )
            .await
            .context("unpacking the backup into the volume")?;
        Ok(())
    }
    .await;
    let options = bollard::query_parameters::RemoveContainerOptionsBuilder::new()
        .force(true)
        .build();
    let _ = docker.remove_container(&name, Some(options)).await;
    result
}

/// Gunzip `src` into `dst` on the blocking pool.
async fn gunzip(src: &Path, dst: &Path) -> Result<()> {
    let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let mut decoder = flate2::read::GzDecoder::new(std::fs::File::open(&src)?);
        let mut output = private_file(&dst)?;
        std::io::copy(&mut decoder, &mut output).context("the backup is not a valid gzip file")?;
        output.flush()?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| anyhow!(e))?
}

/// Gzip `src` into `dst` on the blocking pool.
async fn gzip(src: &Path, dst: &Path) -> Result<()> {
    let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let mut input = std::fs::File::open(&src)?;
        let mut encoder =
            flate2::write::GzEncoder::new(private_file(&dst)?, flate2::Compression::default());
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?.flush()?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| anyhow!(e))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_only_travel_as_env() {
        let env = credentials_env(
            &Some("app; rm -rf /".into()),
            &None,
            &Some("p\"w$(x)".into()),
        );
        assert_eq!(env, vec!["FN_DB=app; rm -rf /", "FN_PASSWORD=p\"w$(x)"]);
        // The scripts reference values only as quoted variables.
        for script in [PG_DUMP, MYSQL_DUMP, PG_RESTORE, MYSQL_RESTORE] {
            assert!(script.contains("\"${FN_DB:-"));
            assert!(!script.contains("eval"));
        }
    }

    #[tokio::test]
    async fn gzip_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("a"), dir.path().join("a.gz"));
        std::fs::write(&src, b"hello backup").unwrap();
        gzip(&src, &dst).await.unwrap();
        let back = dir.path().join("b");
        gunzip(&dst, &back).await.unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), b"hello backup");
        let mut out = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(std::fs::File::open(&dst).unwrap()),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, "hello backup");
    }
}
