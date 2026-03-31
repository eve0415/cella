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
        let result = validate_run_services(
            &[&path],
            &["app".to_string(), "db".to_string()],
        );
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
        let err = validate_run_services(
            &[&path],
            &["app".to_string(), "cache".to_string()],
        )
        .unwrap_err();
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
}
