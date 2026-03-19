use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::resolve::resolve_config;
use cella_credential_proxy::daemon;
use cella_docker::{
    CellaDockerError, ContainerState, DockerClient, ExecOptions, FileToUpload, MountConfig,
    container_labels, container_name, lifecycle, update_remote_user_uid,
};
use cella_env::git_credential::{credential_proxy_pid_path, credential_proxy_socket_path};

use super::image::ensure_image;

/// Start a dev container for the current workspace.
#[derive(Args)]
pub struct UpArgs {
    /// Rebuild the container image before starting.
    #[arg(long)]
    rebuild: bool,

    /// Do not use cache when building the image.
    #[arg(long)]
    build_no_cache: bool,

    /// Remove existing container before starting.
    #[arg(long)]
    remove_existing_container: bool,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    file: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

/// Output format for container commands.
#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

impl UpArgs {
    #[allow(clippy::too_many_lines)]
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = if let Some(ref wf) = self.workspace_folder {
            wf.canonicalize().unwrap_or_else(|_| wf.clone())
        } else {
            std::env::current_dir()?
        };

        let remove_container = self.rebuild || self.remove_existing_container;

        // 1. Resolve config
        info!("Resolving devcontainer config...");
        let resolved = resolve_config(&cwd, self.file.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());
        let remote_user = config
            .get("remoteUser")
            .and_then(|v| v.as_str())
            .unwrap_or("root");

        // 2. Connect to Docker
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };
        client.ping().await?;

        // 3. Check for existing container + handle --rebuild / --remove-existing-container
        let container_nm = container_name(&resolved.workspace_root, config_name);
        let existing = client.find_container(&resolved.workspace_root).await?;

        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder = config.get("workspaceFolder").and_then(|v| v.as_str());

        if let Some(container) = existing {
            match (&container.state, remove_container) {
                (ContainerState::Running, false) if !self.build_no_cache => {
                    // Re-inject env forwarding (git config + SSH files may have changed)
                    let env_fwd = cella_env::prepare_env_forwarding(config, remote_user);
                    inject_post_start(&client, &container.id, &env_fwd.post_start, remote_user)
                        .await;

                    // Already running -- run postAttachCommand and exit
                    if let Some(cmd) = config.get("postAttachCommand") {
                        lifecycle::run_lifecycle_phase(
                            &client,
                            &container.id,
                            "postAttachCommand",
                            cmd,
                            Some(remote_user),
                            &remote_env,
                            workspace_folder,
                        )
                        .await?;
                    }

                    output_result(
                        &self.output,
                        "existing",
                        &container.id,
                        remote_user,
                        workspace_folder.unwrap_or("/workspaces"),
                    );
                    return Ok(());
                }
                (ContainerState::Running, true) => {
                    info!("Stopping container for rebuild...");
                    client.stop_container(&container.id).await?;
                    client.remove_container(&container.id, false).await?;
                }
                (ContainerState::Stopped, false) => {
                    // Start existing stopped container
                    if let Some(old_hash) = &container.config_hash
                        && *old_hash != resolved.config_hash
                    {
                        eprintln!(
                            "\x1b[33mWARNING:\x1b[0m Config has changed since this container was created."
                        );
                        eprintln!(
                            "  Run `cella up --rebuild` to recreate with the updated config."
                        );
                    }

                    client.start_container(&container.id).await?;
                    verify_container_running(&client, &container.id).await?;

                    // Re-inject env forwarding on restart
                    let env_fwd = cella_env::prepare_env_forwarding(config, remote_user);
                    inject_post_start(&client, &container.id, &env_fwd.post_start, remote_user)
                        .await;

                    if let Some(cmd) = config.get("postStartCommand") {
                        lifecycle::run_lifecycle_phase(
                            &client,
                            &container.id,
                            "postStartCommand",
                            cmd,
                            Some(remote_user),
                            &remote_env,
                            workspace_folder,
                        )
                        .await?;
                    }

                    if let Some(cmd) = config.get("postAttachCommand") {
                        lifecycle::run_lifecycle_phase(
                            &client,
                            &container.id,
                            "postAttachCommand",
                            cmd,
                            Some(remote_user),
                            &remote_env,
                            workspace_folder,
                        )
                        .await?;
                    }

                    output_result(
                        &self.output,
                        "started",
                        &container.id,
                        remote_user,
                        workspace_folder.unwrap_or("/workspaces"),
                    );
                    return Ok(());
                }
                (_, true) => {
                    // Rebuild: stop if running, then remove
                    if container.state == ContainerState::Running {
                        client.stop_container(&container.id).await?;
                    }
                    client.remove_container(&container.id, false).await?;
                }
                _ => {
                    // Other state (Created, Removing, etc.) -- remove and recreate
                    let _ = client.remove_container(&container.id, false).await;
                }
            }
        }

        // 4. Run initializeCommand on host (runs every invocation per spec)
        if let Some(init_cmd) = config.get("initializeCommand") {
            run_host_command("initializeCommand", init_cmd)?;
        }

        // 5. Ensure image (with optional features layer)
        let (img_name, resolved_features) = ensure_image(
            &client,
            config,
            &resolved.workspace_root,
            config_name,
            &resolved.config_path,
            self.build_no_cache,
        )
        .await?;

        // 6. Inspect image env for merging with user containerEnv
        let image_env = client.inspect_image_env(&img_name).await?;

        // 6.5. Prepare environment forwarding (SSH agent, git config, credential proxy)
        let env_fwd = cella_env::prepare_env_forwarding(config, remote_user);

        // 6.6. Ensure credential proxy daemon is running (if host has git credentials)
        ensure_credential_proxy();

        // 7. Create container
        let mut labels = container_labels(
            &resolved.workspace_root,
            &resolved.config_path,
            &resolved.config_hash,
        );

        // Store exec metadata labels for fast exec/shell without config re-resolution
        labels.insert("dev.cella.remote_user".to_string(), remote_user.to_string());
        labels.insert(
            "dev.cella.workspace_folder".to_string(),
            workspace_folder.unwrap_or("/workspaces").to_string(),
        );
        if !remote_env.is_empty() {
            labels.insert(
                "dev.cella.remote_env".to_string(),
                serde_json::to_string(&remote_env).unwrap_or_default(),
            );
        }

        if let Some(ref rf) = resolved_features {
            labels.insert(
                "devcontainer.metadata".to_string(),
                rf.metadata_label.clone(),
            );
        }

        let feature_config = resolved_features.as_ref().map(|r| &r.container_config);

        let mut create_opts = cella_docker::config_map::map_config(
            config,
            &container_nm,
            &img_name,
            labels,
            &resolved.workspace_root,
            feature_config,
            &image_env,
        );

        // Merge forwarding mounts
        for m in &env_fwd.mounts {
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: m.source.clone(),
                target: m.target.clone(),
                consistency: None,
            });
        }

        // Merge forwarding env vars
        if !env_fwd.env.is_empty() {
            let fwd_env: Vec<String> = env_fwd
                .env
                .iter()
                .map(|e| format!("{}={}", e.key, e.value))
                .collect();

            if create_opts.env.is_empty() {
                // No user containerEnv was set (Docker would use image env).
                // Now we have forwarding env, so explicitly set image_env + fwd_env.
                create_opts.env = image_env.clone();
            }
            create_opts.env.extend(fwd_env);
        }

        let container_id = client.create_container(&create_opts).await?;

        // 8. Start container
        client.start_container(&container_id).await?;
        verify_container_running(&client, &container_id).await?;

        // 9. updateRemoteUserUID
        let update_uid = config
            .get("updateRemoteUserUID")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        if update_uid
            && remote_user != "root"
            && let Err(e) = update_remote_user_uid(
                &client,
                &container_id,
                remote_user,
                &resolved.workspace_root,
            )
            .await
        {
            warn!("Failed to update remote user UID: {e}");
        }

        // 9.5. Inject post-start environment forwarding (SSH files, git config, credential helper)
        inject_post_start(&client, &container_id, &env_fwd.post_start, remote_user).await;

        // 10-14. Lifecycle commands (first create)
        let lifecycle_phases = [
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ];

        for phase in lifecycle_phases {
            // Feature lifecycle commands run first (features don't have updateContentCommand)
            if let Some(ref rf) = resolved_features {
                let feature_cmds = match phase {
                    "onCreateCommand" => &rf.container_config.lifecycle.on_create,
                    "postCreateCommand" => &rf.container_config.lifecycle.post_create,
                    "postStartCommand" => &rf.container_config.lifecycle.post_start,
                    "postAttachCommand" => &rf.container_config.lifecycle.post_attach,
                    _ => &Vec::new(),
                };
                for cmd in feature_cmds {
                    lifecycle::run_lifecycle_phase(
                        &client,
                        &container_id,
                        phase,
                        cmd,
                        Some(remote_user),
                        &create_opts.remote_env,
                        workspace_folder,
                    )
                    .await?;
                }
            }

            // Then user lifecycle commands
            if let Some(cmd) = config.get(phase) {
                lifecycle::run_lifecycle_phase(
                    &client,
                    &container_id,
                    phase,
                    cmd,
                    Some(remote_user),
                    &create_opts.remote_env,
                    workspace_folder,
                )
                .await?;
            }
        }

        // 15. Output
        output_result(
            &self.output,
            "created",
            &container_id,
            remote_user,
            workspace_folder.unwrap_or("/workspaces"),
        );

        Ok(())
    }
}

fn run_host_command(
    phase: &str,
    value: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Running {phase} on host");

    match value {
        serde_json::Value::String(s) => {
            run_single_host_command(phase, &["sh", "-c", s])?;
        }
        serde_json::Value::Array(arr) => {
            let cmd: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if !cmd.is_empty() {
                let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
                run_single_host_command(phase, &refs)?;
            }
        }
        serde_json::Value::Object(map) => {
            for (name, v) in map {
                info!("{phase} [{name}]");
                match v {
                    serde_json::Value::String(s) => {
                        run_single_host_command(phase, &["sh", "-c", s])?;
                    }
                    serde_json::Value::Array(arr) => {
                        let cmd: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        if !cmd.is_empty() {
                            let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
                            run_single_host_command(phase, &refs)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
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

fn map_env_object(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

async fn verify_container_running(
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

fn output_result(
    format: &OutputFormat,
    outcome: &str,
    container_id: &str,
    remote_user: &str,
    workspace_folder: &str,
) {
    match format {
        OutputFormat::Text => {
            let short_id = &container_id[..12.min(container_id.len())];
            eprintln!("Container {outcome}. ID: {short_id} Workspace: {workspace_folder}");
        }
        OutputFormat::Json => {
            let output = json!({
                "outcome": outcome,
                "containerId": container_id,
                "remoteUser": remote_user,
                "remoteWorkspaceFolder": workspace_folder,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_default()
            );
        }
    }
}

/// Inject post-start environment forwarding into a running container.
///
/// Uploads SSH config files, sets git config, and installs credential helper.
/// Never fails — individual steps log warnings and are skipped on error.
async fn inject_post_start(
    client: &DockerClient,
    container_id: &str,
    post_start: &cella_env::PostStartInjection,
    remote_user: &str,
) {
    // Upload SSH config files
    if !post_start.file_uploads.is_empty() {
        // Create .ssh directory with correct permissions
        let ssh_dir = cella_env::ssh_config::remote_ssh_dir(remote_user);
        let mkdir_result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        format!("mkdir -p {ssh_dir} && chmod 700 {ssh_dir}"),
                    ],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        if let Err(e) = mkdir_result {
            warn!("Failed to create .ssh directory: {e}");
        }

        let docker_files: Vec<FileToUpload> = post_start
            .file_uploads
            .iter()
            .map(|f| FileToUpload {
                path: f.container_path.clone(),
                content: f.content.clone(),
                mode: f.mode,
            })
            .collect();

        if let Err(e) = client.upload_files(container_id, &docker_files).await {
            warn!("Failed to upload SSH config files: {e}");
        } else {
            // Fix ownership
            let _ = client
                .exec_command(
                    container_id,
                    &ExecOptions {
                        cmd: vec![
                            "chown".to_string(),
                            "-R".to_string(),
                            format!("{remote_user}:{remote_user}"),
                            ssh_dir,
                        ],
                        user: Some("root".to_string()),
                        env: None,
                        working_dir: None,
                    },
                )
                .await;
        }
    }

    // Install credential helper script
    if let Some(ref helper) = post_start.credential_helper {
        let helper_file = FileToUpload {
            path: helper.container_path.clone(),
            content: helper.content.clone(),
            mode: helper.mode,
        };
        if let Err(e) = client.upload_files(container_id, &[helper_file]).await {
            warn!("Failed to install credential helper: {e}");
        }
    }

    // Set git config inside container
    if !post_start.git_config_commands.is_empty() {
        for cmd in &post_start.git_config_commands {
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
                    // git probably not installed in container
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
}

/// Ensure the credential proxy daemon is running.
///
/// Starts it as a background process if not already running.
/// Logs a warning and continues if it can't be started.
fn ensure_credential_proxy() {
    let Some(socket_path) = credential_proxy_socket_path() else {
        return;
    };
    let Some(pid_path) = credential_proxy_pid_path() else {
        return;
    };

    if let Err(e) = daemon::ensure_daemon_running(&socket_path, &pid_path) {
        warn!("Failed to start credential proxy daemon: {e}");
    }
}
