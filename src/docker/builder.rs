//! Git clone + image build, via the host `git`, `docker` (BuildKit) and `railpack` CLIs.
//! Arguments are always passed as separate argv entries, never through a shell, and
//! credentials only travel through environment variables (argv is visible in `ps`).

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::run_logged;
use crate::executor::BuildMethod;

const CLONE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct BuildRequest<'a> {
    pub app_id: &'a str,
    pub app_name: &'a str,
    pub deployment_id: &'a str,
    pub repo_url: &'a str,
    pub branch: &'a str,
    /// Access token (https URLs) or private deploy key (ssh URLs).
    pub credential: Option<&'a str>,
    pub method: BuildMethod,
}

pub struct BuildOutput {
    pub image_ref: String,
    pub commit_sha: String,
}

/// Image tag for a built deployment: `forgenerd/<app_name>:<deployment_id>`.
pub fn image_tag(app_name: &str, deployment_id: &str) -> String {
    format!("forgenerd/{app_name}:{deployment_id}")
}

pub async fn build(
    builds_dir: &Path,
    req: &BuildRequest<'_>,
    log: &mpsc::Sender<String>,
) -> Result<BuildOutput> {
    tokio::fs::create_dir_all(builds_dir).await?;
    let workdir = builds_dir.join(req.app_id);
    let key_file = builds_dir.join(format!("{}.key", req.app_id));
    // Fresh shallow clone every time; the directory is removed afterwards to save disk.
    let _ = tokio::fs::remove_dir_all(&workdir).await;
    let result = clone_and_build(builds_dir, &workdir, &key_file, req, log).await;
    let _ = tokio::fs::remove_dir_all(&workdir).await;
    let _ = tokio::fs::remove_file(&key_file).await;
    result
}

async fn clone_and_build(
    builds_dir: &Path,
    workdir: &Path,
    key_file: &Path,
    req: &BuildRequest<'_>,
    log: &mpsc::Sender<String>,
) -> Result<BuildOutput> {
    let _ = log
        .send(format!("cloning {} (branch {})", req.repo_url, req.branch))
        .await;
    let shallow = clone_command(builds_dir, workdir, key_file, req, true)?;
    if let Err(err) = run_logged(shallow, log, CLONE_TIMEOUT).await {
        // Servers on git's "dumb" HTTP protocol cannot serve shallow clones.
        let _ = log
            .send(format!(
                "shallow clone failed ({err:#}); retrying with a full clone"
            ))
            .await;
        let _ = tokio::fs::remove_dir_all(workdir).await;
        let full = clone_command(builds_dir, workdir, key_file, req, false)?;
        run_logged(full, log, CLONE_TIMEOUT)
            .await
            .context("git clone failed")?;
    }

    let commit_sha = rev_parse_head(workdir).await?;
    let _ = log.send(format!("building commit {commit_sha}")).await;

    let tag = image_tag(req.app_name, req.deployment_id);
    match req.method {
        BuildMethod::Dockerfile => {
            if !workdir.join("Dockerfile").exists() {
                bail!(
                    "no Dockerfile at the repository root; set build_method to \"railpack\" for zero-config builds"
                );
            }
            let mut docker = Command::new("docker");
            docker
                .args(["build", "--progress=plain"])
                .arg("--label")
                .arg(format!("forgenerd.app_id={}", req.app_id))
                .args(["-t", &tag, "--"])
                .arg(workdir)
                .env("DOCKER_BUILDKIT", "1");
            run_logged(docker, log, BUILD_TIMEOUT)
                .await
                .context("docker build failed")?;
        }
        BuildMethod::Railpack => {
            let mut railpack = Command::new("railpack");
            railpack.arg("build").arg(workdir).args(["--name", &tag]);
            run_logged(railpack, log, BUILD_TIMEOUT).await.map_err(|err| {
                if is_missing_binary(&err) {
                    anyhow!("railpack is not installed on this node (it also needs a BuildKit daemon via BUILDKIT_HOST)")
                } else {
                    err.context("railpack build failed")
                }
            })?;
        }
    }
    Ok(BuildOutput {
        image_ref: tag,
        commit_sha,
    })
}

fn clone_command(
    builds_dir: &Path,
    workdir: &Path,
    key_file: &Path,
    req: &BuildRequest<'_>,
    shallow: bool,
) -> Result<Command> {
    let mut git = Command::new("git");
    git.args([
        "-c",
        "protocol.file.allow=never",
        "-c",
        "protocol.ext.allow=never",
        "clone",
    ]);
    if shallow {
        git.args(["--depth", "1"]);
    }
    git.args([
        "--single-branch",
        "--no-tags",
        "--branch",
        req.branch,
        "--",
        req.repo_url,
    ])
    .arg(workdir)
    .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(credential) = req.credential {
        configure_credential(&mut git, builds_dir, key_file, req.repo_url, credential)?;
    }
    Ok(git)
}

/// HTTPS: send the token as a Basic auth header via `GIT_CONFIG_*` env vars.
/// SSH: write the deploy key to a 0600 file and point `GIT_SSH_COMMAND` at it.
fn configure_credential(
    git: &mut Command,
    builds_dir: &Path,
    key_file: &Path,
    repo_url: &str,
    credential: &str,
) -> Result<()> {
    if repo_url.starts_with("https://") {
        let basic = STANDARD.encode(format!("x-access-token:{credential}"));
        git.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Basic {basic}"),
            );
        return Ok(());
    }
    let mut key = credential.trim().to_string();
    key.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(key_file)
        .with_context(|| format!("writing deploy key {}", key_file.display()))?;
    std::io::Write::write_all(&mut file, key.as_bytes())?;
    let known_hosts = builds_dir.join("known_hosts");
    git.env(
        "GIT_SSH_COMMAND",
        format!(
            "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
            shell_quote(key_file)?,
            shell_quote(&known_hosts)?
        ),
    );
    Ok(())
}

/// Single-quote a path for `GIT_SSH_COMMAND` (which git runs through a shell).
fn shell_quote(path: &Path) -> Result<String> {
    let s = path.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?;
    if s.contains('\'') {
        bail!("path contains a quote: {s}");
    }
    Ok(format!("'{s}'"))
}

async fn rev_parse_head(workdir: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .context("git rev-parse")?;
    if !output.status.success() {
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_missing_binary(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// Directory where builds happen: `<data_dir>/builds`.
pub fn builds_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_rejects_quotes() {
        assert_eq!(
            shell_quote(Path::new("/var/lib/x.key")).unwrap(),
            "'/var/lib/x.key'"
        );
        assert!(shell_quote(Path::new("/tmp/it's")).is_err());
    }
}
