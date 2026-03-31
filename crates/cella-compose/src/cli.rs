//! Shell out to the `docker compose` V2 CLI.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use serde::Deserialize;

use crate::config::ResolvedComposeConfig;
use crate::error::CellaComposeError;
use crate::project::ComposeProject;

/// A service status entry from `docker compose ps --format json`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ComposeServiceStatus {
    /// Service name.
    pub service: String,
    /// Container name.
    pub name: String,
    /// Container state (e.g., "running", "exited").
    pub state: String,
    /// Published ports (e.g., `"0.0.0.0:3000->3000/tcp"`).
    #[serde(default)]
    pub publishers: Vec<ComposePortPublisher>,
}

/// A published port from compose ps output.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ComposePortPublisher {
    /// Target port inside the container.
    pub target_port: u16,
    /// Published port on the host.
    pub published_port: u16,
    /// Protocol (tcp/udp).
    pub protocol: String,
    /// URL/address the port is published on.
    #[serde(rename = "URL", default)]
    pub url: String,
}

/// Wrapper around the `docker compose` CLI.
///
/// Builds command lines with the correct project name, file flags, and
/// working directory, then executes them.
pub struct ComposeCommand {
    project_name: String,
    compose_files: Vec<PathBuf>,
    override_file: Option<PathBuf>,
    working_dir: PathBuf,
}

impl ComposeCommand {
    /// Create a new compose command context from a project.
    pub fn new(project: &ComposeProject) -> Self {
        Self {
            project_name: project.project_name.clone(),
            compose_files: project.compose_files.clone(),
            override_file: Some(project.override_file.clone()),
            working_dir: project.config_dir.clone(),
        }
    }

    /// Create a compose command without the override file.
    ///
    /// Used for operations that run before the override file has been written
    /// (e.g., `docker compose config` during feature resolution).
    pub fn without_override(project: &ComposeProject) -> Self {
        Self {
            project_name: project.project_name.clone(),
            compose_files: project.compose_files.clone(),
            override_file: None,
            working_dir: project.config_dir.clone(),
        }
    }

    /// Create a minimal compose command from just a project name (for teardown).
    pub fn from_project_name(project_name: &str) -> Self {
        Self {
            project_name: project_name.to_string(),
            compose_files: Vec::new(),
            override_file: None,
            working_dir: PathBuf::from("."),
        }
    }

    /// Build the base `docker compose` command with project name and file flags.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new("docker");
        cmd.arg("compose");
        cmd.arg("--project-name").arg(&self.project_name);
        for f in &self.compose_files {
            cmd.arg("-f").arg(f);
        }
        if let Some(ref ov) = self.override_file {
            cmd.arg("-f").arg(ov);
        }
        cmd.current_dir(&self.working_dir);
        cmd
    }

    /// Run `docker compose up -d [--build] [services...]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the `docker compose` CLI is not found or if the
    /// command exits with a non-zero status.
    pub async fn up(
        &self,
        services: Option<&[String]>,
        build: bool,
    ) -> Result<(), CellaComposeError> {
        let mut cmd = self.base_command();
        cmd.arg("up").arg("-d");
        if build {
            cmd.arg("--build");
        }
        if let Some(svcs) = services {
            for s in svcs {
                cmd.arg(s);
            }
        }
        self.run_streaming(cmd, "up").await
    }

    /// Run `docker compose down`.
    ///
    /// # Errors
    ///
    /// Returns an error if the `docker compose` CLI is not found or if the
    /// command exits with a non-zero status.
    pub async fn down(&self) -> Result<(), CellaComposeError> {
        let mut cmd = self.base_command();
        cmd.arg("down");
        self.run_streaming(cmd, "down").await
    }

    /// Run `docker compose build [services...]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the `docker compose` CLI is not found or if the
    /// command exits with a non-zero status.
    pub async fn build(&self, services: Option<&[String]>) -> Result<(), CellaComposeError> {
        let mut cmd = self.base_command();
        cmd.arg("build");
        if let Some(svcs) = services {
            for s in svcs {
                cmd.arg(s);
            }
        }
        self.run_streaming(cmd, "build").await
    }

    /// Run `docker compose logs [--follow] [--tail N] [services...]`.
    ///
    /// This streams output directly to the terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if the `docker compose` CLI is not found or if the
    /// command exits with a non-zero status.
    pub async fn logs(
        &self,
        follow: bool,
        tail: u32,
        services: Option<&[String]>,
    ) -> Result<(), CellaComposeError> {
        let mut cmd = self.base_command();
        cmd.arg("logs");
        if follow {
            cmd.arg("-f");
        }
        cmd.arg("--tail").arg(tail.to_string());
        if let Some(svcs) = services {
            for s in svcs {
                cmd.arg(s);
            }
        }
        // For logs, inherit stdio for real-time output
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());

        debug!("Running: docker compose logs");
        let status = cmd
            .status()
            .await
            .map_err(|e| CellaComposeError::CliNotFound {
                message: format!("failed to execute docker compose: {e}"),
            })?;

        if !status.success() {
            return Err(CellaComposeError::ComposeFailed {
                exit_code: status.code().unwrap_or(-1),
                stderr: "see output above".to_string(),
            });
        }
        Ok(())
    }

    /// Run `docker compose config --format json` and parse the resolved output.
    ///
    /// This resolves all variable interpolation, extends, profiles, and file
    /// merging, returning the fully expanded compose configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the `docker compose` CLI is not found, the command
    /// fails, or the JSON output cannot be parsed.
    pub async fn config(&self) -> Result<ResolvedComposeConfig, CellaComposeError> {
        let mut cmd = self.base_command();
        cmd.arg("config").arg("--format").arg("json");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        debug!("Running: docker compose config --format json");
        let output = cmd
            .output()
            .await
            .map_err(|e| CellaComposeError::CliNotFound {
                message: format!("failed to execute docker compose: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CellaComposeError::ComposeFailed {
                exit_code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&stdout).map_err(|e| CellaComposeError::ConfigParseFailed {
            message: format!("failed to parse compose config JSON: {e}"),
        })
    }

    /// Run `docker compose ps --format json` and return parsed service statuses.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI is not found or the command fails.
    pub async fn ps_json(&self) -> Result<Vec<ComposeServiceStatus>, CellaComposeError> {
        let mut cmd = self.base_command();
        cmd.arg("ps").arg("--format").arg("json");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        debug!("Running: docker compose ps --format json");
        let output = cmd
            .output()
            .await
            .map_err(|e| CellaComposeError::CliNotFound {
                message: format!("failed to execute docker compose: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CellaComposeError::ComposeFailed {
                exit_code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // docker compose ps --format json may output one JSON object per line
        // (NDJSON) or a JSON array depending on compose version.
        let trimmed = stdout.trim();
        if trimmed.starts_with('[') {
            serde_json::from_str(trimmed).map_err(|e| CellaComposeError::ConfigParseFailed {
                message: format!("failed to parse compose ps JSON: {e}"),
            })
        } else {
            // NDJSON: one JSON object per line
            trimmed
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    serde_json::from_str(line).map_err(|e| CellaComposeError::ConfigParseFailed {
                        message: format!("failed to parse compose ps JSON line: {e}"),
                    })
                })
                .collect()
        }
    }

    /// Execute a command and stream stderr to the terminal, capturing it for errors.
    async fn run_streaming(&self, mut cmd: Command, action: &str) -> Result<(), CellaComposeError> {
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        debug!("Running: docker compose {action}");
        let mut child = cmd.spawn().map_err(|e| CellaComposeError::CliNotFound {
            message: format!("failed to execute docker compose: {e}"),
        })?;

        // Stream stderr line-by-line to the terminal
        let stderr_handle = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut lines = Vec::new();
            if let Some(stderr) = stderr_handle {
                let reader = BufReader::new(stderr);
                let mut line_reader = reader.lines();
                while let Ok(Some(line)) = line_reader.next_line().await {
                    eprintln!("{line}");
                    lines.push(line);
                }
            }
            lines
        });

        // Also drain stdout
        let stdout_handle = child.stdout.take();
        let stdout_task = tokio::spawn(async move {
            if let Some(stdout) = stdout_handle {
                let reader = BufReader::new(stdout);
                let mut line_reader = reader.lines();
                while let Ok(Some(line)) = line_reader.next_line().await {
                    eprintln!("{line}");
                }
            }
        });

        let status = child
            .wait()
            .await
            .map_err(|e| CellaComposeError::CliNotFound {
                message: format!("failed to wait on docker compose: {e}"),
            })?;

        let stderr_lines = stderr_task.await.unwrap_or_default();
        let _ = stdout_task.await;

        if !status.success() {
            let stderr = stderr_lines.join("\n");
            return Err(CellaComposeError::ComposeFailed {
                exit_code: status.code().unwrap_or(-1),
                stderr,
            });
        }

        Ok(())
    }
}

/// Check that `docker compose` V2 is available.
///
/// # Errors
///
/// Returns an error if the `docker compose` CLI is not installed or if the
/// version command fails.
pub async fn check_compose_cli() -> Result<String, CellaComposeError> {
    let output = Command::new("docker")
        .args(["compose", "version"])
        .output()
        .await
        .map_err(|e| CellaComposeError::CliNotFound {
            message: format!("failed to run `docker compose version`: {e}"),
        })?;

    if !output.status.success() {
        return Err(CellaComposeError::CliNotFound {
            message: "docker compose V2 not found (is it installed as a Docker CLI plugin?)"
                .to_string(),
        });
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        warn!("docker compose version returned empty string");
    }
    Ok(version)
}

/// Parse a semantic version triple from `docker compose version` output.
///
/// Accepts formats like:
/// - `"Docker Compose version v2.29.1"`
/// - `"Docker Compose version 2.17.0"`
/// - `"v2.17.0"`
///
/// Returns `None` if the version cannot be parsed.
pub fn parse_compose_version(version_str: &str) -> Option<(u32, u32, u32)> {
    // Find a version-like pattern: optional 'v' prefix followed by digit.digit.digit
    let version_part = version_str.split_whitespace().find(|s| {
        let stripped = s.strip_prefix('v').unwrap_or(s);
        stripped.starts_with(|c: char| c.is_ascii_digit()) && stripped.contains('.')
    })?;
    let version_part = version_part.strip_prefix('v').unwrap_or(version_part);
    let mut parts = version_part.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|p| {
        // Handle versions like "2.17.0-beta.1" — take only the numeric prefix.
        let numeric: String = p.chars().take_while(char::is_ascii_digit).collect();
        numeric.parse().ok()
    })?;
    Some((major, minor, patch))
}

/// Check that Docker Compose supports `additional_contexts` (>= 2.17.0).
///
/// This is required for compose builds with devcontainer features, which use
/// named build contexts to provide feature content separately from the
/// service's original build context.
///
/// # Errors
///
/// Returns [`CellaComposeError::UnsupportedVersion`] if the detected version
/// is older than 2.17.0, or [`CellaComposeError::CliNotFound`] if docker
/// compose is not available.
pub async fn check_compose_features_support() -> Result<(), CellaComposeError> {
    let version_str = check_compose_cli().await?;
    let version = parse_compose_version(&version_str);

    match version {
        Some((major, minor, _)) if major > 2 || (major == 2 && minor >= 17) => Ok(()),
        Some((major, minor, patch)) => Err(CellaComposeError::UnsupportedVersion {
            required: "2.17.0".to_string(),
            found: format!("{major}.{minor}.{patch}"),
            feature: "devcontainer features with compose builds".to_string(),
        }),
        None => {
            // Can't parse version — proceed optimistically, Docker will error if unsupported.
            warn!("could not parse Docker Compose version from: {version_str}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_command_structure() {
        let project = ComposeProject {
            project_name: "cella-myapp-abc12345".to_string(),
            compose_files: vec![
                PathBuf::from("/workspace/.devcontainer/../docker-compose.yml"),
                PathBuf::from("/workspace/.devcontainer/../docker-compose.dev.yml"),
            ],
            override_file: PathBuf::from(
                "/home/user/.cella/compose/cella-myapp-abc12345/docker-compose.cella.yml",
            ),
            primary_service: "app".to_string(),
            run_services: None,
            shutdown_action: crate::project::ShutdownAction::StopCompose,
            override_command: false,
            workspace_folder: "/workspaces/myapp".to_string(),
            config_dir: PathBuf::from("/workspace/.devcontainer"),
            workspace_root: PathBuf::from("/workspace"),
            config_hash: "abc123".to_string(),
        };

        let cmd = ComposeCommand::new(&project);
        let base = cmd.base_command();
        let program = base.as_std().get_program();
        assert_eq!(program, "docker");

        let args: Vec<_> = base.as_std().get_args().collect();
        assert_eq!(args[0], "compose");
        assert_eq!(args[1], "--project-name");
        assert_eq!(args[2], "cella-myapp-abc12345");
        assert_eq!(args[3], "-f");
        // File paths follow
    }

    #[test]
    fn without_override_excludes_override_file() {
        let project = ComposeProject {
            project_name: "cella-myapp-abc12345".to_string(),
            compose_files: vec![PathBuf::from("/workspace/docker-compose.yml")],
            override_file: PathBuf::from(
                "/home/user/.cella/compose/cella-myapp-abc12345/docker-compose.cella.yml",
            ),
            primary_service: "app".to_string(),
            run_services: None,
            shutdown_action: crate::project::ShutdownAction::StopCompose,
            override_command: false,
            workspace_folder: "/workspaces/myapp".to_string(),
            config_dir: PathBuf::from("/workspace/.devcontainer"),
            workspace_root: PathBuf::from("/workspace"),
            config_hash: "abc123".to_string(),
        };

        let cmd = ComposeCommand::without_override(&project);
        let base = cmd.base_command();
        let args: Vec<_> = base.as_std().get_args().collect();
        // Should have: compose --project-name NAME -f compose_file (no override)
        assert_eq!(args[0], "compose");
        assert_eq!(args[1], "--project-name");
        assert_eq!(args[2], "cella-myapp-abc12345");
        assert_eq!(args[3], "-f");
        assert_eq!(args[4], "/workspace/docker-compose.yml");
        assert_eq!(args.len(), 5); // No override file
    }

    #[test]
    fn parse_compose_version_full() {
        assert_eq!(
            parse_compose_version("Docker Compose version v2.29.1"),
            Some((2, 29, 1))
        );
    }

    #[test]
    fn parse_compose_version_no_prefix() {
        assert_eq!(
            parse_compose_version("Docker Compose version 2.17.0"),
            Some((2, 17, 0))
        );
    }

    #[test]
    fn parse_compose_version_short() {
        assert_eq!(parse_compose_version("v2.17.0"), Some((2, 17, 0)));
    }

    #[test]
    fn parse_compose_version_prerelease() {
        assert_eq!(
            parse_compose_version("Docker Compose version v2.17.0-beta.1"),
            Some((2, 17, 0))
        );
    }

    #[test]
    fn parse_compose_version_garbage() {
        assert_eq!(parse_compose_version("not a version"), None);
    }

    #[test]
    fn parse_compose_version_empty() {
        assert_eq!(parse_compose_version(""), None);
    }

    #[test]
    fn from_project_name_minimal() {
        let cmd = ComposeCommand::from_project_name("cella-test-12345678");
        let base = cmd.base_command();
        let args: Vec<_> = base.as_std().get_args().collect();
        assert_eq!(args[0], "compose");
        assert_eq!(args[1], "--project-name");
        assert_eq!(args[2], "cella-test-12345678");
        // No -f flags
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn from_project_name_has_no_compose_files() {
        let cmd = ComposeCommand::from_project_name("cella-test");
        let base = cmd.base_command();
        let args: Vec<_> = base.as_std().get_args().collect();
        // Should only have: compose --project-name cella-test
        assert!(!args.iter().any(|a| *a == "-f"), "expected no -f flags");
    }

    #[test]
    fn without_override_multiple_compose_files() {
        let project = ComposeProject {
            project_name: "cella-multi-abc12345".to_string(),
            compose_files: vec![
                PathBuf::from("/workspace/docker-compose.yml"),
                PathBuf::from("/workspace/docker-compose.dev.yml"),
                PathBuf::from("/workspace/docker-compose.test.yml"),
            ],
            override_file: PathBuf::from(
                "/home/user/.cella/compose/cella-multi-abc12345/docker-compose.cella.yml",
            ),
            primary_service: "app".to_string(),
            run_services: None,
            shutdown_action: crate::project::ShutdownAction::StopCompose,
            override_command: false,
            workspace_folder: "/workspaces/myapp".to_string(),
            config_dir: PathBuf::from("/workspace/.devcontainer"),
            workspace_root: PathBuf::from("/workspace"),
            config_hash: "abc123".to_string(),
        };

        let cmd = ComposeCommand::without_override(&project);
        let base = cmd.base_command();
        let args: Vec<_> = base.as_std().get_args().collect();

        // Count -f flags — should be exactly 3 (one per compose file, no override)
        let f_count = args.iter().filter(|a| **a == "-f").count();
        assert_eq!(f_count, 3);
        assert!(args.contains(&std::ffi::OsStr::new("/workspace/docker-compose.yml")));
        assert!(args.contains(&std::ffi::OsStr::new("/workspace/docker-compose.dev.yml")));
        assert!(args.contains(&std::ffi::OsStr::new("/workspace/docker-compose.test.yml")));
        // Override file must NOT appear
        assert!(!args.iter().any(|a| a.to_string_lossy().contains("cella.yml")));
    }

    #[test]
    fn new_includes_override_file() {
        let project = ComposeProject {
            project_name: "cella-ov-abc12345".to_string(),
            compose_files: vec![PathBuf::from("/workspace/docker-compose.yml")],
            override_file: PathBuf::from(
                "/home/user/.cella/compose/cella-ov-abc12345/docker-compose.cella.yml",
            ),
            primary_service: "app".to_string(),
            run_services: None,
            shutdown_action: crate::project::ShutdownAction::StopCompose,
            override_command: false,
            workspace_folder: "/workspaces/myapp".to_string(),
            config_dir: PathBuf::from("/workspace/.devcontainer"),
            workspace_root: PathBuf::from("/workspace"),
            config_hash: "abc123".to_string(),
        };

        let cmd = ComposeCommand::new(&project);
        let base = cmd.base_command();
        let args: Vec<_> = base.as_std().get_args().collect();

        // Override file should be the last -f argument
        let last_f_idx = args.iter().rposition(|a| *a == "-f").expect("no -f flag found");
        let override_arg = &args[last_f_idx + 1];
        assert!(
            override_arg.to_string_lossy().contains("docker-compose.cella.yml"),
            "override file should be last -f arg, got: {override_arg:?}"
        );
    }

    #[test]
    fn parse_compose_version_two_part() {
        // Only major.minor — missing patch should return None
        assert_eq!(
            parse_compose_version("Docker Compose version v2.17"),
            None
        );
    }

    #[test]
    fn parse_compose_version_very_large() {
        assert_eq!(
            parse_compose_version("Docker Compose version v99.99.99"),
            Some((99, 99, 99))
        );
    }
}
