//! Security policy for user-supplied Docker Compose files.
//!
//! Compose can ask Docker for nearly anything (host mounts, privileged mode, host
//! namespaces, other projects' volumes), and forgenerd talks to Docker as root. Checks
//! run twice:
//!
//! 1. [`check_raw`] on the YAML as written, before `docker compose config` runs: rejects
//!    keys that read host files during config (`env_file`, `extends`, `include`, `build`,
//!    file-based `secrets`/`configs`) or that cannot be undone by interpolation.
//! 2. [`check_normalized`] on the interpolated output of `docker compose config`, so that
//!    `${VAR}` tricks (e.g. `"${SRC}:/data"` with `SRC=/etc`) are caught after expansion.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};

pub const MAX_YAML_BYTES: usize = 256 * 1024;
const MAX_SERVICES: usize = 50;

/// Top-level keys accepted in a compose file (besides `x-*` extensions).
const ALLOWED_TOP: &[&str] = &["services", "volumes", "networks", "version"];

/// Service keys that escape isolation, read host files, or impersonate infrastructure.
const FORBIDDEN_SERVICE_KEYS: &[(&str, &str)] = &[
    (
        "build",
        "images must be pre-built (use a git app to build from source)",
    ),
    ("extends", "reads other files from the host"),
    (
        "env_file",
        "reads files from the host; use the app's environment variables",
    ),
    ("container_name", "container names are managed by forgenerd"),
    (
        "network_mode",
        "services must stay on their project network",
    ),
    ("pid", "host namespaces are not allowed"),
    ("ipc", "host namespaces are not allowed"),
    ("uts", "host namespaces are not allowed"),
    ("userns_mode", "host namespaces are not allowed"),
    ("cgroup", "host namespaces are not allowed"),
    ("cgroup_parent", "custom cgroups are not allowed"),
    ("devices", "host devices are not allowed"),
    ("device_cgroup_rules", "host devices are not allowed"),
    ("cap_add", "extra kernel capabilities are not allowed"),
    ("security_opt", "security options are not allowed"),
    ("runtime", "custom runtimes are not allowed"),
    (
        "volumes_from",
        "mounting other containers' volumes is not allowed",
    ),
    (
        "external_links",
        "linking containers outside the stack is not allowed",
    ),
    (
        "secrets",
        "file-based secrets read host files; use environment variables",
    ),
    (
        "configs",
        "file-based configs read host files; use environment variables",
    ),
];

fn deny(msg: impl Into<String>) -> AppError {
    AppError::validation(format!("compose: {}", msg.into()))
}

/// Parse YAML (with `<<` merge keys applied) into JSON for uniform checking.
pub fn parse(yaml: &str) -> AppResult<Value> {
    if yaml.len() > MAX_YAML_BYTES {
        return Err(deny(format!(
            "file is larger than {} KiB",
            MAX_YAML_BYTES / 1024
        )));
    }
    let mut doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| deny(format!("invalid YAML: {e}")))?;
    doc.apply_merge()
        .map_err(|e| deny(format!("invalid YAML merge: {e}")))?;
    serde_json::to_value(doc).map_err(|e| deny(format!("unsupported YAML: {e}")))
}

fn services(doc: &Value) -> AppResult<&Map<String, Value>> {
    doc.get("services")
        .and_then(Value::as_object)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| deny("at least one service is required"))
}

/// Structural checks on the YAML as written. Returns the service names.
pub fn check_raw(yaml: &str) -> AppResult<Vec<String>> {
    let doc = parse(yaml)?;
    let root = doc
        .as_object()
        .ok_or_else(|| deny("top level must be a mapping"))?;
    for key in root.keys() {
        if !ALLOWED_TOP.contains(&key.as_str()) && !key.starts_with("x-") {
            return Err(deny(format!("top-level `{key}` is not supported")));
        }
    }

    let services = services(&doc)?;
    if services.len() > MAX_SERVICES {
        return Err(deny(format!("at most {MAX_SERVICES} services are allowed")));
    }
    for (name, service) in services {
        super::name(name).map_err(|_| {
            deny(format!(
                "service name `{name}` must be a lowercase DNS label"
            ))
        })?;
        let service = service
            .as_object()
            .ok_or_else(|| deny(format!("service `{name}` must be a mapping")))?;
        for (key, why) in FORBIDDEN_SERVICE_KEYS {
            if service.contains_key(*key) {
                return Err(deny(format!(
                    "service `{name}`: `{key}` is not allowed ({why})"
                )));
            }
        }
        if let Some(privileged) = service.get("privileged")
            && privileged != &Value::Bool(false)
        {
            return Err(deny(format!(
                "service `{name}`: privileged mode is not allowed"
            )));
        }
    }

    check_top_level_resources(&doc, "volumes", &["driver", "labels"], Some("local"))?;
    check_top_level_resources(
        &doc,
        "networks",
        &["driver", "labels", "internal", "ipam", "enable_ipv6"],
        Some("bridge"),
    )?;
    Ok(services.keys().cloned().collect())
}

/// Top-level volumes/networks must be project-scoped: no `external`, no `name` (which
/// could attach another project's or forgenerd's own volumes, e.g. Caddy's TLS keys),
/// no `driver_opts` (a `local` volume with `o: bind` mounts a host path).
fn check_top_level_resources(
    doc: &Value,
    kind: &str,
    allowed: &[&str],
    driver: Option<&str>,
) -> AppResult<()> {
    let Some(resources) = doc.get(kind) else {
        return Ok(());
    };
    let resources = resources
        .as_object()
        .ok_or_else(|| deny(format!("top-level `{kind}` must be a mapping")))?;
    for (name, spec) in resources {
        let Some(spec) = spec.as_object() else {
            if spec.is_null() {
                continue;
            }
            return Err(deny(format!("{kind} `{name}` must be a mapping")));
        };
        for key in spec.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(deny(format!(
                    "{kind} `{name}`: `{key}` is not allowed (only project-scoped {kind} are supported)"
                )));
            }
        }
        if let (Some(expected), Some(actual)) = (driver, spec.get("driver"))
            && actual.as_str() != Some(expected)
        {
            return Err(deny(format!(
                "{kind} `{name}`: only the `{expected}` driver is allowed"
            )));
        }
    }
    Ok(())
}

/// Checks on the normalized, interpolated output of `docker compose config --format json`.
pub fn check_normalized(doc: &Value) -> AppResult<()> {
    let declared: HashSet<&str> = doc
        .get("volumes")
        .and_then(Value::as_object)
        .map(|v| v.keys().map(String::as_str).collect())
        .unwrap_or_default();

    for (name, service) in services(doc)? {
        if service.get("privileged").and_then(Value::as_bool) == Some(true) {
            return Err(deny(format!(
                "service `{name}`: privileged mode is not allowed"
            )));
        }
        if service.get("network_mode").is_some() {
            return Err(deny(format!(
                "service `{name}`: network_mode is not allowed"
            )));
        }
        for volume in service
            .get("volumes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let kind = volume
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let source = volume
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "tmpfs" => {}
                // Anonymous volumes have no source; named ones must be declared top-level
                // (and are therefore project-scoped).
                "volume" if source.is_empty() || declared.contains(source) => {}
                "volume" => {
                    return Err(deny(format!(
                        "service `{name}`: volume `{source}` must be declared under top-level `volumes`"
                    )));
                }
                other => {
                    return Err(deny(format!(
                        "service `{name}`: `{other}` mounts are not allowed (host paths cannot be mounted)"
                    )));
                }
            }
        }
        let labels = service.get("labels").and_then(Value::as_object);
        if labels.is_some_and(|l| l.keys().any(|k| k.starts_with("forgenerd."))) {
            return Err(deny(format!(
                "service `{name}`: `forgenerd.*` labels are reserved"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
x-common: &common
  restart: always
services:
  web:
    <<: *common
    image: nginx:alpine
    ports: ["8080:80"]
    volumes: [data:/data, /cache]
    environment:
      DB: ${DB_URL}
  db:
    image: postgres:18-alpine
    privileged: false
volumes:
  data:
networks:
  backend:
    driver: bridge
"#;

    fn raw_err(yaml: &str) -> String {
        check_raw(yaml).unwrap_err().to_string()
    }

    #[test]
    fn accepts_typical_stacks() {
        let mut services = check_raw(OK).unwrap();
        services.sort();
        assert_eq!(services, vec!["db", "web"]);
    }

    #[test]
    fn rejects_dangerous_service_keys() {
        for (snippet, expect) in [
            ("build: .", "build"),
            ("env_file: /etc/shadow", "env_file"),
            ("container_name: forgenerd-caddy", "container_name"),
            ("network_mode: host", "network_mode"),
            ("pid: host", "pid"),
            ("cap_add: [SYS_ADMIN]", "cap_add"),
            ("privileged: true", "privileged"),
            ("privileged: ${P}", "privileged"),
            ("devices: [/dev/sda]", "devices"),
            ("security_opt: [seccomp=unconfined]", "security_opt"),
            ("volumes_from: [other]", "volumes_from"),
            ("extends: {file: /etc/x.yml, service: a}", "extends"),
            ("secrets: [s]", "secrets"),
        ] {
            let yaml = format!("services:\n  a:\n    image: x\n    {snippet}\n");
            let err = raw_err(&yaml);
            assert!(err.contains(expect), "{snippet}: {err}");
        }
    }

    #[test]
    fn rejects_unscoped_resources_and_top_level_keys() {
        for (yaml, expect) in [
            (
                "include: [/etc/x.yml]\nservices: {a: {image: x}}",
                "include",
            ),
            (
                "secrets: {s: {file: /etc/shadow}}\nservices: {a: {image: x}}",
                "secrets",
            ),
            (
                "services: {a: {image: x}}\nvolumes: {v: {external: true}}",
                "external",
            ),
            (
                "services: {a: {image: x}}\nvolumes: {v: {name: forgenerd-caddy-data}}",
                "name",
            ),
            (
                "services: {a: {image: x}}\nvolumes: {v: {driver_opts: {type: none, o: bind, device: /}}}",
                "driver_opts",
            ),
            (
                "services: {a: {image: x}}\nvolumes: {v: {driver: nfs}}",
                "driver",
            ),
            (
                "services: {a: {image: x}}\nnetworks: {n: {external: true}}",
                "external",
            ),
            ("services: {}", "at least one service"),
            ("services: {Web: {image: x}}", "DNS label"),
            ("- a\n- b", "mapping"),
        ] {
            let err = raw_err(yaml);
            assert!(err.contains(expect), "{yaml}: {err}");
        }
        assert!(raw_err(&"a".repeat(MAX_YAML_BYTES + 1)).contains("larger"));
    }

    #[test]
    fn normalized_rejects_interpolated_host_mounts() {
        // What `docker compose config` produces for "${SRC}:/data" with SRC=/etc.
        let doc = serde_json::json!({
            "services": {"a": {"image": "x", "volumes": [{"type": "bind", "source": "/etc", "target": "/data"}]}}
        });
        assert!(
            check_normalized(&doc)
                .unwrap_err()
                .to_string()
                .contains("bind")
        );

        let undeclared = serde_json::json!({
            "services": {"a": {"volumes": [{"type": "volume", "source": "forgenerd-caddy-data", "target": "/d"}]}}
        });
        assert!(check_normalized(&undeclared).is_err());

        let ok = serde_json::json!({
            "services": {"a": {"volumes": [
                {"type": "volume", "source": "data", "target": "/d"},
                {"type": "volume", "target": "/anon"},
                {"type": "tmpfs", "target": "/tmp"}
            ]}},
            "volumes": {"data": {"name": "forgenerd-x_data"}}
        });
        assert!(check_normalized(&ok).is_ok());

        let labels =
            serde_json::json!({"services": {"a": {"labels": {"forgenerd.managed": "true"}}}});
        assert!(check_normalized(&labels).is_err());
    }
}
