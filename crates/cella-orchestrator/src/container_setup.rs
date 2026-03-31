//! Container setup helpers extracted from the CLI `up` command.
//!
//! These are pure business-logic functions that operate on a [`DockerClient`]
//! and devcontainer config values. They have no CLI or progress-reporting
//! dependencies.

use cella_docker::{
    CellaDockerError, ContainerState, DockerClient, ExecOptions, ExecResult, FileToUpload,
};
use tracing::{debug, info, warn};

// ── Host commands (initializeCommand) ─────────────────────────────────────

/// Run an `initializeCommand` (or similar host-side lifecycle command).
///
/// Supports string, array, and object (named) forms per the devcontainer spec.
///
/// # Errors
///
/// Returns an error if any individual command exits with a non-zero status.
pub fn run_host_command(
    phase: &str,
    value: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Running {phase} on host");

    match value {
        serde_json::Value::String(s) => {
            run_single_host_command(phase, &["sh", "-c", s])?;
        }
        serde_json::Value::Array(arr) => {
            run_json_array_command(phase, arr)?;
        }
        serde_json::Value::Object(map) => {
            for (name, v) in map {
                info!("{phase} [{name}]");
                match v {
                    serde_json::Value::String(s) => {
                        run_single_host_command(phase, &["sh", "-c", s])?;
                    }
                    serde_json::Value::Array(arr) => {
                        run_json_array_command(phase, arr)?;
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn run_json_array_command(
    phase: &str,
    arr: &[serde_json::Value],
) -> Result<(), Box<dyn std::error::Error>> {
    let cmd: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if !cmd.is_empty() {
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        run_single_host_command(phase, &refs)?;
    }
    Ok(())
}

fn run_single_host_command(phase: &str, cmd: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    if cmd.is_empty() {
        return Ok(());
    }

    let status = std::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .status()?;

    if !status.success() {
        return Err(format!(
            "{phase} failed with exit code {}",
            status.code().unwrap_or(-1)
        )
        .into());
    }

    Ok(())
}

// ── Pure conversion helpers ───────────────────────────────────────────────

/// Convert a JSON `remoteEnv` object to a vec of `KEY=value` strings.
pub fn map_env_object(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert [`cella_env::FileUpload`] items to [`cella_docker::FileToUpload`].
pub fn convert_uploads(uploads: &[cella_env::FileUpload]) -> Vec<FileToUpload> {
    uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect()
}

/// Resolve the remote user from config and image metadata.
///
/// Priority: `remoteUser` (config) > `containerUser` (config) > `remoteUser`
/// (image metadata) > `containerUser` (image metadata) > `fallback` (typically
/// Docker USER or `"root"`).
pub fn resolve_remote_user(
    config: &serde_json::Value,
    image_meta_user: Option<&cella_features::ImageMetadataUserInfo>,
    fallback: &str,
) -> String {
    config
        .get("remoteUser")
        .and_then(|v| v.as_str())
        .or_else(|| config.get("containerUser").and_then(|v| v.as_str()))
        .or_else(|| image_meta_user.and_then(|m| m.remote_user.as_deref()))
        .or_else(|| image_meta_user.and_then(|m| m.container_user.as_deref()))
        .unwrap_or(fallback)
        .to_string()
}

// ── Container verification ────────────────────────────────────────────────

/// Verify that a container is in the `Running` state. Returns the Docker
/// error (with log tail) if it has already exited.
///
/// # Errors
///
/// Returns an error if the container is not running.
pub async fn verify_container_running(
    client: &DockerClient,
    container_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = client.inspect_container(container_id).await?;
    if info.state != ContainerState::Running {
        let logs = client.container_logs(container_id, 20).await?;
        return Err(CellaDockerError::ContainerExitedImmediately {
            exit_code: info.exit_code.unwrap_or(-1),
            logs_tail: logs,
        }
        .into());
    }
    Ok(())
}

// ── Daemon env query ──────────────────────────────────────────────────────

/// Query the daemon for control port and auth token, returning env vars to
/// inject into the container.
pub async fn query_daemon_env(container_nm: &str) -> Vec<String> {
    if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
        && mgmt_sock.exists()
    {
        let status_resp = cella_daemon::management::send_management_request(
            &mgmt_sock,
            &cella_port::protocol::ManagementRequest::QueryStatus,
        )
        .await;

        if let Ok(cella_port::protocol::ManagementResponse::Status {
            control_port,
            control_token,
            ..
        }) = &status_resp
        {
            return vec![
                format!("CELLA_DAEMON_ADDR=host.docker.internal:{control_port}"),
                format!("CELLA_DAEMON_TOKEN={control_token}"),
                format!("CELLA_CONTAINER_NAME={container_nm}"),
            ];
        }
    }
    vec![]
}

// ── In-container operation helpers ────────────────────────────────────────

/// Create a directory inside the container with the given mode (as root).
///
/// # Errors
///
/// Returns an error if the exec fails.
pub async fn mkdir_in_container(
    client: &DockerClient,
    container_id: &str,
    dir: &str,
    mode: u32,
) -> Result<ExecResult, CellaDockerError> {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("mkdir -p {dir} && chmod {mode:o} {dir}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
}

/// Recursively chown a directory inside the container.
pub async fn chown_in_container(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    dir: &str,
) {
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "chown".to_string(),
                    "-R".to_string(),
                    format!("{remote_user}:{remote_user}"),
                    dir.to_string(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Create a directory, upload files, and fix ownership.
///
/// Returns `true` on success, `false` on any step failure.
pub async fn upload_to_container(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    dir: &str,
    uploads: &[cella_env::FileUpload],
    context_label: &str,
) -> bool {
    if let Err(e) = mkdir_in_container(client, container_id, dir, 0o700).await {
        warn!("Failed to create {context_label} directory: {e}");
        return false;
    }

    let docker_files = convert_uploads(uploads);
    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        warn!("Failed to upload {context_label} files: {e}");
    }

    chown_in_container(client, container_id, remote_user, dir).await;
    true
}

/// Check if a config already exists in the container (runs a test command).
pub async fn config_exists_in_container(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    check_cmd: &[String],
) -> bool {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: check_cmd.to_vec(),
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0)
}

// ── SSH / Git setup ───────────────────────────────────────────────────────

/// Inject post-start environment forwarding into a running container.
///
/// Uploads SSH config files, sets git config, and runs privileged root
/// commands. Never fails -- individual steps log warnings and are skipped
/// on error.
pub async fn inject_post_start(
    client: &DockerClient,
    container_id: &str,
    post_start: &cella_env::PostStartInjection,
    remote_user: &str,
) {
    upload_ssh_files(client, container_id, &post_start.file_uploads, remote_user).await;
    apply_git_config(
        client,
        container_id,
        &post_start.git_config_commands,
        remote_user,
    )
    .await;

    // Execute privileged commands (e.g., CA trust store updates) as root.
    for cmd in &post_start.root_commands {
        let result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: cmd.clone(),
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        match result {
            Ok(r) if r.exit_code != 0 => {
                warn!(
                    "Root command failed (exit {}): {}",
                    r.exit_code,
                    r.stderr.trim()
                );
            }
            Err(e) => {
                warn!("Failed to exec root command: {e}");
            }
            _ => {}
        }
    }
}

/// Upload SSH config files to the container's `.ssh` directory.
async fn upload_ssh_files(
    client: &DockerClient,
    container_id: &str,
    uploads: &[cella_env::FileUpload],
    remote_user: &str,
) {
    if uploads.is_empty() {
        return;
    }

    let ssh_dir = cella_env::ssh_config::remote_ssh_dir(remote_user);
    if let Err(e) = mkdir_in_container(client, container_id, &ssh_dir, 0o700).await {
        warn!("Failed to create .ssh directory: {e}");
        return;
    }

    let docker_files = convert_uploads(uploads);
    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        warn!("Failed to upload SSH config files: {e}");
    } else {
        chown_in_container(client, container_id, remote_user, &ssh_dir).await;
    }
}

/// Apply git config commands inside the container.
async fn apply_git_config(
    client: &DockerClient,
    container_id: &str,
    commands: &[Vec<String>],
    remote_user: &str,
) {
    for cmd in commands {
        let result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: cmd.clone(),
                    user: Some(remote_user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        match result {
            Ok(r) if r.exit_code != 0 => {
                warn!(
                    "git config failed (exit {}): {}",
                    r.exit_code,
                    r.stderr.trim()
                );
                break;
            }
            Err(e) => {
                warn!("Failed to exec git config: {e}");
                break;
            }
            _ => {}
        }
    }
}

/// Add `/cella/bin` to PATH in the container's shell profile.
///
/// This makes the `cella` CLI (symlinked to the agent binary) discoverable
/// by users and AI agents running inside the container.
pub async fn inject_cella_path(client: &DockerClient, container_id: &str, remote_user: &str) {
    let snippet = r#"
# cella CLI (in-container worktree commands)
if [ -d /cella/bin ] && ! echo "$PATH" | grep -q /cella/bin; then
    export PATH="/cella/bin:$PATH"
fi
"#;
    // Determine home directory
    let home = if remote_user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{remote_user}")
    };

    for profile in &[".bashrc", ".zshrc", ".profile"] {
        let path = format!("{home}/{profile}");
        let cmd = format!(
            "if [ -f '{path}' ] && ! grep -q '/cella/bin' '{path}'; then printf '%s\\n' '{snippet_escaped}' >> '{path}'; fi",
            path = path,
            snippet_escaped = snippet.replace('\'', "'\\''"),
        );
        let _ = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec!["sh".to_string(), "-c".to_string(), cmd],
                    user: Some("root".to_string()),
                    working_dir: None,
                    env: None,
                },
            )
            .await;
    }
}

/// Seed gh CLI credentials into a container.
///
/// Extracts tokens from the host's gh CLI and uploads `hosts.yml` and
/// `config.yml` into the container. Skips silently if gh is not
/// installed/authenticated or if credentials already exist in the container.
pub async fn seed_gh_credentials(
    client: &DockerClient,
    container_id: &str,
    workspace_root: &std::path::Path,
    remote_user: &str,
) {
    let config_dir = cella_env::gh_credential::gh_config_dir_for_user(remote_user);

    if config_exists_in_container(
        client,
        container_id,
        remote_user,
        &cella_env::gh_credential::gh_config_exists_in_container(&config_dir),
    )
    .await
    {
        debug!("gh credentials already present in container, skipping seed");
        return;
    }

    let Some(gh_creds) =
        cella_env::gh_credential::prepare_gh_credentials(workspace_root, remote_user)
    else {
        return;
    };

    if upload_to_container(
        client,
        container_id,
        remote_user,
        &config_dir,
        &gh_creds.file_uploads,
        "gh config",
    )
    .await
    {
        debug!("Seeded gh CLI credentials into container");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_remote_user_from_config() {
        let config = json!({"remoteUser": "vscode"});
        let result = resolve_remote_user(&config, None, "root");
        assert_eq!(result, "vscode");
    }

    #[test]
    fn resolve_remote_user_from_container_user() {
        let config = json!({"containerUser": "node"});
        let result = resolve_remote_user(&config, None, "root");
        assert_eq!(result, "node");
    }

    #[test]
    fn resolve_remote_user_from_image_metadata() {
        let config = json!({});
        let meta = cella_features::ImageMetadataUserInfo {
            remote_user: Some("devuser".to_string()),
            container_user: None,
        };
        let result = resolve_remote_user(&config, Some(&meta), "root");
        assert_eq!(result, "devuser");
    }

    #[test]
    fn resolve_remote_user_fallback() {
        let config = json!({});
        let result = resolve_remote_user(&config, None, "root");
        assert_eq!(result, "root");
    }

    #[test]
    fn resolve_remote_user_priority_order() {
        let config = json!({"remoteUser": "winner", "containerUser": "loser"});
        let meta = cella_features::ImageMetadataUserInfo {
            remote_user: Some("also-loser".to_string()),
            container_user: Some("yet-another-loser".to_string()),
        };
        let result = resolve_remote_user(&config, Some(&meta), "fallback");
        assert_eq!(result, "winner");
    }

    #[test]
    fn map_env_object_basic() {
        let val = json!({"FOO": "bar"});
        let result = map_env_object(Some(&val));
        assert_eq!(result, vec!["FOO=bar"]);
    }

    #[test]
    fn map_env_object_null_values() {
        let val = json!({"KEY": null});
        let result = map_env_object(Some(&val));
        assert_eq!(result, vec!["KEY="]);
    }

    #[test]
    fn convert_uploads_basic() {
        let uploads = vec![cella_env::FileUpload {
            container_path: "/home/user/.config/test".to_string(),
            content: b"hello".to_vec(),
            mode: 0o644,
        }];
        let result = convert_uploads(&uploads);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/home/user/.config/test");
        assert_eq!(result[0].content, b"hello");
        assert_eq!(result[0].mode, 0o644);
    }
}
