//! Minimal Docker Compose YAML parsing for service validation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// The container↔host path pair for a compose service's workspace, or `None`
/// when nothing binds it to a host directory.
///
/// Compose ignores `workspaceMount` — the service's own `volumes:` define the
/// mapping — so assuming `(workspace_folder, workspace_root)` advertises a
/// mapping that may not exist. The agent would then rewrite `projectPath` to an
/// unrelated host directory *and use it as the normalized install-context key*,
/// so a wrong path does not merely mislabel an entry: it creates a distinct
/// merge slot.
///
/// Picks the longest bind whose target covers `workspace_folder` at a component
/// boundary, so a workspace nested inside a broader bind still maps correctly.
/// Named volumes and anonymous volumes yield `None`.
///
/// # Errors
///
/// Returns an error if any compose file cannot be read or contains invalid YAML.
pub fn workspace_bind_for_service(
    compose_files: &[impl AsRef<Path>],
    service: &str,
    workspace_folder: &str,
) -> Result<Option<(String, String)>, CellaComposeError> {
    // Compose resolves relative bind sources against the project directory,
    // which is the directory of the first compose file.
    let project_dir = compose_files
        .first()
        .and_then(|p| p.as_ref().parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let mut best: Option<(String, String)> = None;
    for path in compose_files {
        let path = path.as_ref();
        let content =
            std::fs::read_to_string(path).map_err(|_| CellaComposeError::FileNotFound {
                path: path.to_path_buf(),
            })?;
        let parsed: ComposeFile = yaml_serde::from_str(&content)
            .map_err(|e| CellaComposeError::YamlParse(e.to_string()))?;
        let Some(volumes) = parsed
            .services
            .get(service)
            .and_then(|s| s.get("volumes"))
            .and_then(|v| v.as_sequence())
        else {
            continue;
        };
        for entry in volumes {
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
            let host = absolutize(&source, &project_dir);
            best = Some((target, host));
        }
    }
    Ok(best)
}

/// Split one `volumes:` entry into `(host source, container target)`, or `None`
/// when it is not a host bind (named/anonymous volume, or an unparseable shape).
fn parse_volume_entry(entry: &yaml_serde::Value) -> Option<(String, String)> {
    // Long syntax: { type: bind, source: ..., target: ... }
    if let Some(map) = entry.as_mapping() {
        let kind = map
            .get("type")
            .and_then(yaml_serde::Value::as_str)
            .unwrap_or("volume");
        if kind != "bind" {
            return None;
        }
        let source = map.get("source").and_then(yaml_serde::Value::as_str)?;
        let target = map.get("target").and_then(yaml_serde::Value::as_str)?;
        return Some((source.to_string(), target.to_string()));
    }

    // Short syntax: "source:target[:mode]". A bare "/target" is an anonymous
    // volume, and a source that is not a path is a named volume.
    let text = entry.as_str()?;
    let mut parts = text.splitn(3, ':');
    let source = parts.next()?;
    let target = parts.next()?;
    if !is_host_path(source) {
        return None;
    }
    Some((source.to_string(), target.to_string()))
}

/// Whether a short-syntax source names a host path rather than a named volume.
fn is_host_path(source: &str) -> bool {
    source.starts_with('/')
        || source.starts_with("./")
        || source.starts_with("../")
        || source.starts_with("~/")
        || source == "."
        || source == ".."
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

/// Resolve a possibly-relative bind source against the compose project dir.
fn absolutize(source: &str, project_dir: &Path) -> String {
    let path = Path::new(source);
    if path.is_absolute() {
        return source.to_string();
    }
    project_dir
        .join(path)
        .canonicalize()
        .unwrap_or_else(|_| project_dir.join(path))
        .to_string_lossy()
        .into_owned()
}

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

    fn write_compose(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
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

    #[test]
    fn workspace_bind_reads_a_short_syntax_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - /host/code:/workspaces/app\n",
        );
        assert_eq!(
            workspace_bind_for_service(&[f], "app", "/workspaces/app").expect("parses"),
            Some(("/workspaces/app".to_string(), "/host/code".to_string()))
        );
    }

    #[test]
    fn workspace_bind_reads_long_syntax_and_ignores_mode_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - type: bind\n        source: /host/code\n        target: /code\n",
        );
        assert_eq!(
            workspace_bind_for_service(&[f], "app", "/code/sub").expect("parses"),
            Some(("/code".to_string(), "/host/code".to_string())),
            "a workspace nested inside a broader bind still maps"
        );
    }

    /// The defect this exists for: a named volume advertises no host directory,
    /// so no pair may be produced.
    #[test]
    fn workspace_bind_absent_for_a_named_volume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - code-data:/workspaces/app\n",
        );
        assert_eq!(
            workspace_bind_for_service(&[f], "app", "/workspaces/app").expect("parses"),
            None
        );
    }

    #[test]
    fn workspace_bind_absent_when_nothing_covers_the_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - /host/other:/elsewhere\n",
        );
        assert_eq!(
            workspace_bind_for_service(&[f], "app", "/workspaces/app").expect("parses"),
            None
        );
    }

    /// A sibling target sharing a textual prefix must not be treated as covering
    /// the workspace.
    #[test]
    fn workspace_bind_respects_component_boundaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - /host/other:/workspaces/app-extra\n",
        );
        assert_eq!(
            workspace_bind_for_service(&[f], "app", "/workspaces/app").expect("parses"),
            None
        );
    }

    #[test]
    fn workspace_bind_prefers_the_longest_covering_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - /host/root:/code\n      - /host/inner:/code/app\n",
        );
        assert_eq!(
            workspace_bind_for_service(&[f], "app", "/code/app").expect("parses"),
            Some(("/code/app".to_string(), "/host/inner".to_string()))
        );
    }

    #[test]
    fn workspace_bind_resolves_a_relative_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        let f = write_compose(
            &dir,
            "docker-compose.yaml",
            "services:\n  app:\n    volumes:\n      - ./src:/workspaces/app\n",
        );
        let (target, source) = workspace_bind_for_service(&[f], "app", "/workspaces/app")
            .expect("parses")
            .expect("bind found");
        assert_eq!(target, "/workspaces/app");
        assert!(
            source.ends_with("src"),
            "a relative source resolves against the project dir: {source}"
        );
        assert!(Path::new(&source).is_absolute());
    }
}
