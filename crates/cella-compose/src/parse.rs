//! Minimal Docker Compose YAML parsing for service validation.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::CellaComposeError;

/// Minimal representation of a Docker Compose file.
///
/// Only the `services` key is parsed — everything else is ignored.
#[derive(Deserialize)]
struct ComposeFile {
    #[serde(default)]
    services: HashMap<String, yaml_serde::Value>,
}

/// The container↔host path pair for a resolved compose service's workspace, or
/// `None` when nothing binds it to a host directory.
///
/// Compose ignores `workspaceMount` — the service's own `volumes:` define the
/// mapping — so assuming `(workspace_folder, workspace_root)` advertises a
/// mapping that may not exist. The agent would then rewrite `projectPath` to an
/// unrelated host directory *and use it as the normalized install-context key*,
/// so a wrong path does not merely mislabel an entry: it creates a distinct
/// merge slot.
///
/// Takes the *resolved* service (from `docker compose config`) rather than the
/// raw `-f` files, so file merging, `extends`, profiles and interpolation are
/// already applied and bind sources are absolute. Parsing the files
/// independently would miss a later file overriding the same target, and would
/// not see a bind superseded by a named volume.
///
/// Picks the longest bind whose target covers `workspace_folder` at a component
/// boundary, so a workspace nested inside a broader bind still maps correctly.
#[must_use]
pub fn workspace_bind_for_service(
    service: &crate::config::ResolvedService,
    workspace_folder: &str,
) -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    for entry in &service.volumes {
        let Some((source, target)) = parse_volume_entry(entry) else {
            continue;
        };
        if !covers_path(&target, workspace_folder) {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|(existing, _)| existing.len() >= target.len())
        {
            continue;
        }
        best = Some((target, source));
    }
    best
}

/// Split one resolved `volumes:` entry into `(host source, container target)`,
/// or `None` when it is not a host bind.
fn parse_volume_entry(entry: &serde_json::Value) -> Option<(String, String)> {
    // Long syntax: { type: bind, source: ..., target: ... }
    if let Some(map) = entry.as_object() {
        let kind = map
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("volume");
        if kind != "bind" {
            return None;
        }
        let source = map.get("source").and_then(serde_json::Value::as_str)?;
        let target = map.get("target").and_then(serde_json::Value::as_str)?;
        return Some((source.to_string(), target.to_string()));
    }

    // Short syntax: "source:target[:mode]". A bare "/target" is an anonymous
    // volume, and a source that is not a path is a named volume.
    let text = entry.as_str()?;
    let mut parts = text.splitn(3, ':');
    let source = parts.next()?;
    let target = parts.next()?;
    if !source.starts_with('/') {
        // `docker compose config` absolutizes bind sources, so anything else is
        // a named volume.
        return None;
    }
    Some((source.to_string(), target.to_string()))
}

/// Whether `prefix` covers `path` at a component boundary.
fn covers_path(prefix: &str, path: &str) -> bool {
    let prefix = prefix
        .strip_suffix('/')
        .filter(|p| !p.is_empty())
        .unwrap_or(prefix);
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Parse one or more compose files and return the merged set of service names.
/// Parse one or more compose files and return the merged set of service names.
///
/// Service names are deduplicated across files (later files can redefine
/// services from earlier files, matching Docker Compose merge behavior).
///
/// # Errors
///
/// Returns an error if any compose file cannot be read or contains invalid YAML.
pub fn parse_service_names(
    compose_files: &[impl AsRef<Path>],
) -> Result<Vec<String>, CellaComposeError> {
    let mut all_services = Vec::new();
    for path in compose_files {
        let path = path.as_ref();
        let content =
            std::fs::read_to_string(path).map_err(|_| CellaComposeError::FileNotFound {
                path: path.to_path_buf(),
            })?;
        let parsed: ComposeFile = yaml_serde::from_str(&content)
            .map_err(|e| CellaComposeError::YamlParse(e.to_string()))?;
        for name in parsed.services.keys() {
            if !all_services.contains(name) {
                all_services.push(name.clone());
            }
        }
    }
    all_services.sort();
    Ok(all_services)
}

/// Validate that the primary service exists in the compose files.
///
/// # Errors
///
/// Returns an error if the compose files cannot be parsed or the primary
/// service is not found among the defined services.
pub fn validate_primary_service(
    compose_files: &[impl AsRef<Path>],
    primary_service: &str,
) -> Result<(), CellaComposeError> {
    let services = parse_service_names(compose_files)?;
    if !services.iter().any(|s| s == primary_service) {
        return Err(CellaComposeError::ServiceNotFound {
            service: primary_service.to_string(),
            available: services.join(", "),
        });
    }
    Ok(())
}

/// Validate that all `run_services` entries exist in the compose files.
///
/// # Errors
///
/// Returns an error if the compose files cannot be parsed or any of the
/// specified run services are not found among the defined services.
pub fn validate_run_services(
    compose_files: &[impl AsRef<Path>],
    run_services: &[String],
) -> Result<(), CellaComposeError> {
    let services = parse_service_names(compose_files)?;
    for svc in run_services {
        if !services.contains(svc) {
            return Err(CellaComposeError::ServiceNotFound {
                service: svc.clone(),
                available: services.join(", "),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_compose(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parse_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n  db:\n    image: postgres\n",
        );
        let names = parse_service_names(&[&path]).unwrap();
        assert_eq!(names, vec!["app", "db"]);
    }

    #[test]
    fn parse_multiple_files_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n  db:\n    image: postgres\n",
        );
        let p2 = write_compose(
            &dir,
            "docker-compose.dev.yml",
            "services:\n  app:\n    ports:\n      - '3000:3000'\n  redis:\n    image: redis\n",
        );
        let names = parse_service_names(&[&p1, &p2]).unwrap();
        assert_eq!(names, vec!["app", "db", "redis"]);
    }

    #[test]
    fn validate_primary_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n",
        );
        assert!(validate_primary_service(&[&path], "app").is_ok());
    }

    #[test]
    fn validate_primary_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n",
        );
        let err = validate_primary_service(&[&path], "web").unwrap_err();
        assert!(err.to_string().contains("web"));
        assert!(err.to_string().contains("app"));
    }

    #[test]
    fn parse_empty_services() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(&dir, "docker-compose.yml", "services: {}\n");
        let names = parse_service_names(&[&path]).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn parse_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(&dir, "docker-compose.yml", "{{invalid yaml");
        assert!(parse_service_names(&[&path]).is_err());
    }

    #[test]
    fn parse_file_not_found() {
        let result = parse_service_names(&[Path::new("/nonexistent/compose.yml")]);
        assert!(matches!(
            result.unwrap_err(),
            CellaComposeError::FileNotFound { .. }
        ));
    }

    #[test]
    fn validate_run_services_all_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n  db:\n    image: postgres\n  redis:\n    image: redis\n",
        );
        let result = validate_run_services(&[&path], &["app".to_string(), "db".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_run_services_one_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n  db:\n    image: postgres\n",
        );
        let err =
            validate_run_services(&[&path], &["app".to_string(), "cache".to_string()]).unwrap_err();
        assert!(matches!(err, CellaComposeError::ServiceNotFound { .. }));
        let msg = err.to_string();
        assert!(msg.contains("cache"));
    }

    #[test]
    fn validate_run_services_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_compose(
            &dir,
            "docker-compose.yml",
            "services:\n  app:\n    image: node\n",
        );
        let result = validate_run_services(&[&path], &[]);
        assert!(result.is_ok());
    }

    // ── workspace_bind_for_service ─────────────────────────────────────────

    fn service_with(volumes: &serde_json::Value) -> crate::config::ResolvedService {
        crate::config::ResolvedService {
            volumes: volumes.as_array().expect("array").clone(),
            ..Default::default()
        }
    }

    #[test]
    fn workspace_bind_reads_a_short_syntax_bind() {
        let svc = service_with(&serde_json::json!(["/host/code:/workspaces/app"]));
        assert_eq!(
            workspace_bind_for_service(&svc, "/workspaces/app"),
            Some(("/workspaces/app".to_string(), "/host/code".to_string()))
        );
    }

    #[test]
    fn workspace_bind_reads_long_syntax_and_covers_a_nested_folder() {
        let svc = service_with(
            &serde_json::json!([{ "type": "bind", "source": "/host/code", "target": "/code" }]),
        );
        assert_eq!(
            workspace_bind_for_service(&svc, "/code/sub"),
            Some(("/code".to_string(), "/host/code".to_string())),
            "a workspace nested inside a broader bind still maps"
        );
    }

    /// The defect this exists for: a named volume advertises no host directory,
    /// so no pair may be produced.
    #[test]
    fn workspace_bind_absent_for_a_named_volume() {
        let svc = service_with(&serde_json::json!(["code-data:/workspaces/app"]));
        assert_eq!(workspace_bind_for_service(&svc, "/workspaces/app"), None);

        let long = service_with(
            &serde_json::json!([{ "type": "volume", "source": "code", "target": "/workspaces/app" }]),
        );
        assert_eq!(workspace_bind_for_service(&long, "/workspaces/app"), None);
    }

    #[test]
    fn workspace_bind_absent_when_nothing_covers_the_folder() {
        let svc = service_with(&serde_json::json!(["/host/other:/elsewhere"]));
        assert_eq!(workspace_bind_for_service(&svc, "/workspaces/app"), None);
    }

    /// A sibling target sharing a textual prefix must not be treated as covering
    /// the workspace.
    #[test]
    fn workspace_bind_respects_component_boundaries() {
        let svc = service_with(&serde_json::json!(["/host/other:/workspaces/app-extra"]));
        assert_eq!(workspace_bind_for_service(&svc, "/workspaces/app"), None);
    }

    #[test]
    fn workspace_bind_prefers_the_longest_covering_bind() {
        let svc = service_with(&serde_json::json!([
            "/host/root:/code",
            "/host/inner:/code/app"
        ]));
        assert_eq!(
            workspace_bind_for_service(&svc, "/code/app"),
            Some(("/code/app".to_string(), "/host/inner".to_string()))
        );
    }

    #[test]
    fn workspace_bind_ignores_an_anonymous_volume() {
        let svc = service_with(&serde_json::json!(["/workspaces/app"]));
        assert_eq!(workspace_bind_for_service(&svc, "/workspaces/app"), None);
    }
}
