//! Input validation applied before any value reaches Docker, external CLIs, or Caddy.

pub mod compose;

use crate::error::{AppError, AppResult};

fn is_dns_label(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

/// App, volume, and compose service names: `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`.
pub fn name(value: &str) -> AppResult<()> {
    if is_dns_label(value) {
        Ok(())
    } else {
        Err(AppError::validation(
            "name must be 1-63 chars of lowercase letters, digits, or '-', not starting or ending with '-'",
        ))
    }
}

/// Docker image tags: the tag portion after `image:` in an image reference.
pub fn image_tag(value: &str) -> AppResult<()> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && !value.starts_with(['.', '-'])
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(AppError::validation(format!(
            "invalid Docker image tag: {value}"
        )))
    }
}

/// App names also become Docker container names, compose project names and network
/// aliases, so the `forgenerd` prefix (used by the proxy and infrastructure) is reserved.
pub fn app_name(value: &str) -> AppResult<()> {
    name(value)?;
    if value.starts_with("forgenerd") {
        return Err(AppError::validation(
            "app names starting with \"forgenerd\" are reserved",
        ));
    }
    Ok(())
}

/// Lowercase RFC 1123 FQDN with at least two labels.
pub fn hostname(value: &str) -> AppResult<()> {
    let labels: Vec<&str> = value.split('.').collect();
    let ok = value.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| is_dns_label(l))
        && labels
            .last()
            .is_some_and(|tld| !tld.bytes().all(|b| b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(AppError::validation(format!("invalid hostname: {value}")))
    }
}

/// Only `https://`, `ssh://`, or scp-style `git@host:path`. Rejects `file://`, `ext::`,
/// option injection (leading `-`), whitespace, and control characters.
pub fn git_url(value: &str) -> AppResult<()> {
    let err = || AppError::validation("git URL must be https://, ssh://, or git@host:path");
    if value.is_empty()
        || value.len() > 2048
        || value.starts_with('-')
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(err());
    }
    let rest = if let Some(rest) = value.strip_prefix("https://") {
        rest
    } else if let Some(rest) = value.strip_prefix("ssh://") {
        rest
    } else if let Some(rest) = value.strip_prefix("git@") {
        // scp-style: git@host:path
        let (host, path) = rest.split_once(':').ok_or_else(err)?;
        if host.is_empty() || path.is_empty() || host.contains('/') {
            return Err(err());
        }
        return Ok(());
    } else {
        return Err(err());
    };
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or_default();
    if host.is_empty() || rest.len() <= host.len() {
        return Err(err());
    }
    Ok(())
}

/// Branch names following the `git check-ref-format --branch` rules.
pub fn git_branch(value: &str) -> AppResult<()> {
    let bad = value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.ends_with(".lock")
        || value == "@"
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value.split('/').any(|c| c.starts_with('.'))
        || value
            .chars()
            .any(|c| c.is_control() || c == ' ' || "~^:?*[\\".contains(c));
    if bad {
        Err(AppError::validation(format!(
            "invalid branch name: {value}"
        )))
    } else {
        Ok(())
    }
}

/// Absolute container path, not `/`, without `..` components.
pub fn mount_path(value: &str) -> AppResult<()> {
    let ok = value.starts_with('/')
        && value != "/"
        && value.len() <= 4096
        && !value.split('/').any(|c| c == "..")
        && !value.chars().any(|c| c.is_control() || c == ':');
    if ok {
        Ok(())
    } else {
        Err(AppError::validation(format!("invalid mount path: {value}")))
    }
}

/// Docker image reference: `[registry[:port]/]path[:tag][@sha256:<hex>]`.
pub fn image_ref(value: &str) -> AppResult<()> {
    let err = || AppError::validation(format!("invalid image reference: {value}"));
    if value.is_empty() || value.len() > 512 || value.starts_with(['-', '/', '.']) {
        return Err(err());
    }
    let (rest, digest) = match value.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest)),
        None => (value, None),
    };
    if let Some(digest) = digest {
        let hex = digest.strip_prefix("sha256:").ok_or_else(err)?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err());
        }
    }
    let (repo, tag) = match rest.rsplit_once('/') {
        Some((head, last)) => match last.split_once(':') {
            Some((name, tag)) => (format!("{head}/{name}"), Some(tag)),
            None => (rest.to_string(), None),
        },
        None => match rest.split_once(':') {
            Some((name, tag)) => (name.to_string(), Some(tag)),
            None => (rest.to_string(), None),
        },
    };
    if let Some(tag) = tag {
        let ok = !tag.is_empty()
            && tag.len() <= 128
            && !tag.starts_with(['.', '-'])
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !ok {
            return Err(err());
        }
    }
    let mut components = repo.split('/');
    let first = components.next().unwrap_or_default();
    // The first component may be a registry host (contains '.' or ':' or is localhost).
    let is_registry = first.contains(['.', ':']) || first == "localhost";
    let registry_ok = |c: &str| {
        !c.is_empty()
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
    };
    let path_ok = |c: &str| {
        !c.is_empty()
            && c.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && c.as_bytes()[0].is_ascii_alphanumeric()
            && c.as_bytes()[c.len() - 1].is_ascii_alphanumeric()
    };
    let first_ok = if is_registry {
        registry_ok(first)
    } else {
        path_ok(first)
    };
    let path_components: Vec<&str> = components.collect();
    if !first_ok
        || (is_registry && path_components.is_empty())
        || !path_components.iter().all(|c| path_ok(c))
    {
        return Err(err());
    }
    Ok(())
}

/// Environment variable names: `^[A-Za-z_][A-Za-z0-9_]*$`.
pub fn env_key(value: &str) -> AppResult<()> {
    let ok = !value.is_empty()
        && value.len() <= 256
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(AppError::validation(format!(
            "invalid environment variable name: {value}"
        )))
    }
}

/// Environment values: at most 64 KiB, no NUL bytes.
pub fn env_value(key: &str, value: &str) -> AppResult<()> {
    if value.len() > 64 * 1024 || value.contains('\0') {
        Err(AppError::validation(format!(
            "value of {key} is too long or contains NUL"
        )))
    } else {
        Ok(())
    }
}

/// HTTP path for health checks: starts with '/', no whitespace or control characters.
pub fn http_path(value: &str) -> AppResult<()> {
    let ok = value.starts_with('/')
        && value.len() <= 1024
        && !value.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(AppError::validation(format!(
            "invalid healthcheck path: {value}"
        )))
    }
}

/// TCP port a container listens on.
pub fn port(value: i64) -> AppResult<()> {
    if (1..=65535).contains(&value) {
        Ok(())
    } else {
        Err(AppError::validation(
            "container_port must be between 1 and 65535",
        ))
    }
}

/// Memory limit in MiB: at least 16 (Docker's floor is 6), at most 1 TiB.
pub fn memory_limit_mb(value: i64) -> AppResult<()> {
    if (16..=1_048_576).contains(&value) {
        Ok(())
    } else {
        Err(AppError::validation(
            "memory_limit_mb must be between 16 and 1048576",
        ))
    }
}

/// CPU limit in cores, e.g. 0.5 or 2.
pub fn cpu_limit(value: f64) -> AppResult<()> {
    if value.is_finite() && (0.01..=1024.0).contains(&value) {
        Ok(())
    } else {
        Err(AppError::validation(
            "cpu_limit must be between 0.01 and 1024",
        ))
    }
}

pub fn health_timeout_secs(value: i64) -> AppResult<()> {
    if (5..=900).contains(&value) {
        Ok(())
    } else {
        Err(AppError::validation(
            "health_timeout_secs must be between 5 and 900",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_refs() {
        for ok in [
            "nginx",
            "nginx:1.27-alpine",
            "traefik/whoami:v1.10",
            "ghcr.io/org/app:latest",
            "localhost:5000/app",
            "registry.example.com:443/a/b/c:v2",
            &format!("nginx@sha256:{}", "a".repeat(64)),
        ] {
            assert!(image_ref(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Nginx",
            "-nginx",
            "nginx:",
            "nginx:bad tag",
            "a//b",
            "nginx@sha256:xyz",
            "ghcr.io",
            "nginx;rm -rf /",
            "../etc",
        ] {
            assert!(image_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_keys_and_paths() {
        for ok in ["PORT", "_X", "database_url1"] {
            assert!(env_key(ok).is_ok(), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "A=B"] {
            assert!(env_key(bad).is_err(), "{bad}");
        }
        assert!(http_path("/healthz?x=1").is_ok());
        assert!(http_path("healthz").is_err());
        assert!(http_path("/a b").is_err());
        assert!(port(8080).is_ok() && port(0).is_err() && port(70000).is_err());
    }

    #[test]
    fn names() {
        for ok in ["a", "my-api", "app1", "a1-b2"] {
            assert!(name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", "a-", "My", "a_b", "a.b", "a b", &"a".repeat(64)] {
            assert!(name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hostnames() {
        for ok in ["api.example.com", "a.io", "x-1.y.z"] {
            assert!(hostname(ok).is_ok(), "{ok}");
        }
        for bad in [
            "localhost",
            "API.example.com",
            "a..b",
            "-a.com",
            "1.2.3.4",
            "a.com.",
            "a b.com",
            "a{}.com",
        ] {
            assert!(hostname(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn git_urls() {
        for ok in [
            "https://github.com/a/b.git",
            "ssh://git@github.com/a/b.git",
            "git@github.com:a/b.git",
            "https://user@gitlab.example.com:8443/g/p",
        ] {
            assert!(git_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc",
            "ext::sh -c id",
            "-uhttps://x",
            "http://github.com/a/b",
            "https://",
            "https://host",
            "git@:path",
            "https://a.com/b c",
            "",
        ] {
            assert!(git_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn branches() {
        for ok in ["main", "feature/x", "release-1.2", "v1"] {
            assert!(git_branch(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "-x", "a..b", "a.lock", "a b", "a~1", "/a", "a/", "a//b", ".a", "a/.b", "@",
            "a@{b", "a.",
        ] {
            assert!(git_branch(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn mount_paths() {
        for ok in ["/data", "/var/lib/postgresql/data"] {
            assert!(mount_path(ok).is_ok(), "{ok}");
        }
        for bad in ["", "/", "data", "/a/../etc", "/a:b"] {
            assert!(mount_path(bad).is_err(), "{bad}");
        }
    }
}
