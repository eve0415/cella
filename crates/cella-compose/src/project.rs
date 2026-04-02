//! Compose project configuration extracted from a resolved devcontainer config.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::error::CellaComposeError;
use crate::hash::compute_compose_hash;

/// What to do when the user disconnects from the compose project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownAction {
    /// Do nothing — leave services running.
    None,
    /// Stop all compose services (default).
    StopCompose,
}

impl ShutdownAction {
    fn from_str(s: &str) -> Self {
        match s {
            "none" => Self::None,
            _ => Self::StopCompose,
        }
    }
}

/// A fully resolved Docker Compose project derived from a devcontainer config.
#[derive(Debug, Clone)]
pub struct ComposeProject {
    /// Compose project name: `cella-{identifier}-{hash8}`.
    pub project_name: String,
    /// Absolute paths to the user's compose file(s).
    pub compose_files: Vec<PathBuf>,
    /// Path to the cella override file.
    pub override_file: PathBuf,
    /// Primary service name (from devcontainer.json `service`).
    pub primary_service: String,
    /// Services to start (from `runServices`). `None` means all.
    pub run_services: Option<Vec<String>>,
    /// Shutdown action (default: `StopCompose`).
    pub shutdown_action: ShutdownAction,
    /// Whether to override the service entrypoint (default: `false` for compose).
    pub override_command: bool,
    /// Workspace folder inside the primary container.
    pub workspace_folder: String,
    /// Directory containing the devcontainer.json (for relative path resolution).
    pub config_dir: PathBuf,
    /// Workspace root (repo root).
    pub workspace_root: PathBuf,
    /// SHA256 hash of devcontainer.json + all compose files.
    pub config_hash: String,
    /// Docker Compose profiles to activate (`--profile` flags).
    pub profiles: Vec<String>,
    /// Extra env-file paths to pass to docker compose (`--env-file` flags).
    pub env_files: Vec<PathBuf>,
    /// Pull policy for `docker compose up`/`build` (`--pull` flag).
    pub pull_policy: Option<String>,
}

impl ComposeProject {
    /// Build a `ComposeProject` from a resolved devcontainer config.
    ///
    /// Extracts `dockerComposeFile`, `service`, `runServices`, `shutdownAction`,
    /// `overrideCommand`, and `workspaceFolder` from the config JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if required fields (`service`, `workspaceFolder`,
    /// `dockerComposeFile`) are missing or if compose files cannot be resolved.
    pub fn from_resolved(
        config: &serde_json::Value,
        config_path: &Path,
        workspace_root: &Path,
    ) -> Result<Self, CellaComposeError> {
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        // dockerComposeFile: string or array of strings (required)
        let compose_files = extract_compose_files(config, &config_dir)?;

        // service: string (required)
        let primary_service = config
            .get("service")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CellaComposeError::MissingField {
                field: "service".to_string(),
            })?
            .to_string();

        // workspaceFolder: string (required for compose)
        let workspace_folder = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CellaComposeError::MissingField {
                field: "workspaceFolder".to_string(),
            })?
            .to_string();

        // runServices: optional array of strings
        let run_services = config.get("runServices").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
        });

        // shutdownAction: "none" | "stopCompose" (default: "stopCompose")
        let shutdown_action = config
            .get("shutdownAction")
            .and_then(|v| v.as_str())
            .map_or(ShutdownAction::StopCompose, ShutdownAction::from_str);

        // overrideCommand: boolean (default: false for compose)
        let override_command = config
            .get("overrideCommand")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let config_name = config.get("name").and_then(|v| v.as_str());
        let project_name = compose_project_name(workspace_root, config_name);

        let config_hash = compute_compose_hash(config, &compose_files);

        let override_file = compose_override_path(&project_name);

        Ok(Self {
            project_name,
            compose_files,
            override_file,
            primary_service,
            run_services,
            shutdown_action,
            override_command,
            workspace_folder,
            config_dir,
            workspace_root: workspace_root.to_path_buf(),
            config_hash,
            profiles: Vec::new(),
            env_files: Vec::new(),
            pull_policy: None,
        })
    }

    /// Override the project name and recompute the override file path.
    #[must_use]
    pub fn with_project_name(mut self, name: String) -> Self {
        self.override_file = compose_override_path(&name);
        self.project_name = name;
        self
    }

    /// Set compose profiles, env-file paths, and pull policy from CLI arguments.
    ///
    /// Recomputes `config_hash` to include the new options so that changing
    /// e.g. `--profile` triggers container re-creation.
    pub fn set_compose_options(
        &mut self,
        profiles: Vec<String>,
        env_files: Vec<PathBuf>,
        pull_policy: Option<String>,
    ) {
        if !profiles.is_empty() {
            debug!("Compose profiles: {profiles:?}");
        }
        if !env_files.is_empty() {
            debug!("Compose env files: {env_files:?}");
        }
        if let Some(ref policy) = pull_policy {
            debug!("Compose pull policy: {policy}");
        }
        self.profiles = profiles;
        self.env_files = env_files;
        self.pull_policy = pull_policy;

        // Recompute hash to include CLI options that affect the compose project.
        let mut hasher = Sha256::new();
        hasher.update(self.config_hash.as_bytes());
        for p in &self.profiles {
            hasher.update(b"profile:");
            hasher.update(p.as_bytes());
        }
        for ef in &self.env_files {
            hasher.update(b"env_file:");
            hasher.update(ef.to_string_lossy().as_bytes());
        }
        if let Some(ref pp) = self.pull_policy {
            hasher.update(b"pull_policy:");
            hasher.update(pp.as_bytes());
        }
        self.config_hash = hex::encode(hasher.finalize());
    }
}

/// Extract and resolve compose file paths from the config.
///
/// `dockerComposeFile` can be a string or an array of strings.
/// Paths are resolved relative to the devcontainer.json directory.
fn extract_compose_files(
    config: &serde_json::Value,
    config_dir: &Path,
) -> Result<Vec<PathBuf>, CellaComposeError> {
    let raw = config
        .get("dockerComposeFile")
        .ok_or_else(|| CellaComposeError::MissingField {
            field: "dockerComposeFile".to_string(),
        })?;

    let paths: Vec<String> = if let Some(s) = raw.as_str() {
        vec![s.to_string()]
    } else if let Some(arr) = raw.as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        return Err(CellaComposeError::MissingField {
            field: "dockerComposeFile".to_string(),
        });
    };

    let mut resolved = Vec::with_capacity(paths.len());
    for p in &paths {
        let abs = config_dir.join(p);
        let canonical = abs.canonicalize().unwrap_or(abs);
        if !canonical.exists() {
            return Err(CellaComposeError::FileNotFound { path: canonical });
        }
        resolved.push(canonical);
    }

    Ok(resolved)
}

/// Generate compose project name: `cella-{identifier}-{hash8}`.
///
/// Uses the same naming pattern as `cella_docker::names::container_name`.
pub fn compose_project_name(workspace_root: &Path, config_name: Option<&str>) -> String {
    let identifier = config_name.map_or_else(
        || {
            workspace_root.file_name().map_or_else(
                || "unnamed".to_string(),
                |n| sanitize_name(&n.to_string_lossy()),
            )
        },
        sanitize_name,
    );
    let hash = workspace_hash(workspace_root);
    format!("cella-{identifier}-{hash}")
}

/// Hash workspace path to short hex (first 8 chars of SHA256).
fn workspace_hash(workspace_root: &Path) -> String {
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let hash = hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()));
    hash[..8].to_string()
}

/// Sanitize a string for Docker project name compatibility.
///
/// Lowercases and replaces invalid characters, then collapses consecutive
/// separator chars (`[._-]`) into a single dash. This ensures the result
/// is valid as a Docker image repository name (which must be lowercase).
fn sanitize_name(s: &str) -> String {
    let sanitized: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive separator chars ([._-]+) into a single dash.
    // Docker path-component separators are [._] | '__' | '-'+, but mixed
    // sequences like "._" or "_-" are invalid. Using '-' is always safe.
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_sep = false;
    for c in sanitized.chars() {
        let is_sep = c == '-' || c == '_' || c == '.';
        if is_sep {
            if !prev_sep {
                result.push('-');
            }
            prev_sep = true;
        } else {
            result.push(c);
            prev_sep = false;
        }
    }

    result.trim_matches('-').to_string()
}

/// Compute the path for the cella override compose file.
fn compose_override_path(project_name: &str) -> PathBuf {
    let data_dir = cella_data_dir().join("compose").join(project_name);
    data_dir.join("docker-compose.cella.yml")
}

/// Get the cella data directory (`~/.cella/`).
fn cella_data_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join(".cella")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_name_deterministic() {
        let path = PathBuf::from("/tmp/my-project");
        let n1 = compose_project_name(&path, Some("myapp"));
        let n2 = compose_project_name(&path, Some("myapp"));
        assert_eq!(n1, n2);
    }

    #[test]
    fn project_name_with_config_name() {
        let path = PathBuf::from("/tmp/my-project");
        let name = compose_project_name(&path, Some("myapp"));
        assert!(name.starts_with("cella-myapp-"));
        assert_eq!(name.len(), "cella-myapp-".len() + 8);
    }

    #[test]
    fn project_name_from_folder() {
        let path = PathBuf::from("/tmp/my-project");
        let name = compose_project_name(&path, None);
        assert!(name.starts_with("cella-my-project-"));
    }

    #[test]
    fn override_file_path() {
        let path = compose_override_path("cella-myapp-abc12345");
        assert!(
            path.to_string_lossy()
                .contains("compose/cella-myapp-abc12345/docker-compose.cella.yml")
        );
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(sanitize_name("my app@v2"), "my-app-v2");
    }

    #[test]
    fn sanitize_collapses_dashes() {
        assert_eq!(sanitize_name("a---b"), "a-b");
    }

    #[test]
    fn sanitize_lowercases() {
        assert_eq!(sanitize_name("ActivityManager"), "activitymanager");
    }

    #[test]
    fn project_name_always_lowercase() {
        let path = PathBuf::from("/tmp/MyProject");
        let name = compose_project_name(&path, Some("MyApp"));
        assert_eq!(name, name.to_lowercase());
    }

    #[test]
    fn from_resolved_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let compose_path = dir.path().join("docker-compose.yml");
        std::fs::write(&compose_path, "services:\n  app:\n    image: node\n").unwrap();

        let config_path = dir.path().join("devcontainer.json");
        let config = serde_json::json!({
            "dockerComposeFile": "docker-compose.yml",
            "service": "app",
            "workspaceFolder": "/workspaces/myapp"
        });

        let project = ComposeProject::from_resolved(&config, &config_path, dir.path()).unwrap();
        assert_eq!(project.primary_service, "app");
        assert_eq!(project.workspace_folder, "/workspaces/myapp");
        assert!(!project.override_command);
        assert_eq!(project.shutdown_action, ShutdownAction::StopCompose);
        assert!(project.run_services.is_none());
        assert_eq!(project.compose_files.len(), 1);
    }

    #[test]
    fn from_resolved_array_compose_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  app:\n    image: node\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("docker-compose.dev.yml"),
            "services:\n  app:\n    ports:\n      - '3000:3000'\n",
        )
        .unwrap();

        let config_path = dir.path().join("devcontainer.json");
        let config = serde_json::json!({
            "dockerComposeFile": ["docker-compose.yml", "docker-compose.dev.yml"],
            "service": "app",
            "workspaceFolder": "/workspaces/myapp"
        });

        let project = ComposeProject::from_resolved(&config, &config_path, dir.path()).unwrap();
        assert_eq!(project.compose_files.len(), 2);
    }

    #[test]
    fn from_resolved_missing_service() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  app:\n    image: node\n",
        )
        .unwrap();

        let config_path = dir.path().join("devcontainer.json");
        let config = serde_json::json!({
            "dockerComposeFile": "docker-compose.yml",
            "workspaceFolder": "/workspaces/myapp"
        });
        let err = ComposeProject::from_resolved(&config, &config_path, dir.path()).unwrap_err();
        assert!(matches!(err, CellaComposeError::MissingField { .. }));
    }

    #[test]
    fn sanitize_collapses_mixed_separators() {
        assert_eq!(sanitize_name("a._-b"), "a-b");
    }

    #[test]
    fn sanitize_strips_leading_trailing_dashes() {
        assert_eq!(sanitize_name("-my-app-"), "my-app");
    }

    #[test]
    fn from_resolved_with_run_services() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  app:\n    image: node\n  db:\n    image: postgres\n",
        )
        .unwrap();

        let config_path = dir.path().join("devcontainer.json");
        let config = serde_json::json!({
            "dockerComposeFile": "docker-compose.yml",
            "service": "app",
            "workspaceFolder": "/workspaces/myapp",
            "runServices": ["app", "db"]
        });

        let project = ComposeProject::from_resolved(&config, &config_path, dir.path()).unwrap();
        let run_services = project.run_services.expect("run_services should be Some");
        assert_eq!(run_services, vec!["app", "db"]);
    }

    #[test]
    fn from_resolved_missing_workspace_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  app:\n    image: node\n",
        )
        .unwrap();

        let config_path = dir.path().join("devcontainer.json");
        let config = serde_json::json!({
            "dockerComposeFile": "docker-compose.yml",
            "service": "app"
        });

        let err = ComposeProject::from_resolved(&config, &config_path, dir.path()).unwrap_err();
        assert!(
            matches!(err, CellaComposeError::MissingField { ref field } if field == "workspaceFolder")
        );
    }
}
