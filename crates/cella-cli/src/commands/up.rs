use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::resolve::resolve_config;
use cella_docker::{
    CellaDockerError, ContainerState, DockerClient, ExecOptions, FileToUpload, MountConfig,
    container_labels, container_name, lifecycle, update_remote_user_uid,
};

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
        let is_text = matches!(&self.output, OutputFormat::Text);

        // 1. Resolve config
        if is_text {
            eprintln!("Resolving devcontainer configuration...");
        }
        let resolved = resolve_config(&cwd, self.file.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

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
        let workspace_basename = resolved.workspace_root.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let default_workspace_folder = format!("/workspaces/{workspace_basename}");
        let workspace_folder_str = workspace_folder.unwrap_or(&default_workspace_folder);

        if let Some(container) = existing {
            // Re-resolve remote_user from config (labels may be stale from older containers).
            // Priority: remoteUser > containerUser > image USER > label fallback > "root"
            let remote_user = if let Some(u) = config.get("remoteUser").and_then(|v| v.as_str()) {
                u.to_string()
            } else if let Some(u) = config.get("containerUser").and_then(|v| v.as_str()) {
                u.to_string()
            } else if let Some(ref img) = container.image {
                client
                    .inspect_image_user(img)
                    .await
                    .unwrap_or_else(|_| "root".to_string())
            } else {
                container
                    .labels
                    .get("dev.cella.remote_user")
                    .cloned()
                    .unwrap_or_else(|| "root".to_string())
            };

            let probe_type = config
                .get("userEnvProbe")
                .and_then(|v| v.as_str())
                .unwrap_or("loginInteractiveShell");

            match (&container.state, remove_container) {
                (ContainerState::Running, false) if !self.build_no_cache => {
                    super::ensure_cella_daemon().await;
                    // Re-inject env forwarding (git config + SSH files may have changed)
                    let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user);
                    timed_step(
                        is_text,
                        "Configuring environment...",
                        inject_post_start(
                            &client,
                            &container.id,
                            &env_fwd.post_start,
                            &remote_user,
                        ),
                    )
                    .await;

                    // Claude Code: re-sync auth + ensure installed
                    let cc_settings = cella_config::CellaSettings::load(&resolved.workspace_root);
                    if cc_settings.tools.claude_code.forward_config {
                        resync_claude_auth(&client, &container.id, &remote_user).await;
                    }
                    if cc_settings.tools.claude_code.enabled {
                        install_claude_code(
                            &client,
                            &container.id,
                            &remote_user,
                            &cc_settings.tools.claude_code,
                        )
                        .await;
                    }

                    timed_step(
                        is_text,
                        "Running userEnvProbe...",
                        super::env_cache::probe_and_cache_user_env(
                            &client,
                            &container.id,
                            &remote_user,
                            probe_type,
                        ),
                    )
                    .await;

                    // Already running -- run postAttachCommand from metadata (includes features)
                    let metadata = container.labels.get("devcontainer.metadata");
                    let entries = metadata.map_or_else(
                        || {
                            config
                                .get("postAttachCommand")
                                .filter(|v| !v.is_null())
                                .map(|cmd| {
                                    vec![cella_features::LifecycleEntry {
                                        origin: "devcontainer.json".into(),
                                        command: cmd.clone(),
                                    }]
                                })
                                .unwrap_or_default()
                        },
                        |meta_json| {
                            cella_features::lifecycle_from_metadata_label(
                                meta_json,
                                "postAttachCommand",
                            )
                        },
                    );
                    run_lifecycle_entries(
                        &client,
                        &container.id,
                        "postAttachCommand",
                        &entries,
                        Some(remote_user.as_str()),
                        &remote_env,
                        workspace_folder,
                        is_text,
                    )
                    .await?;

                    output_result(
                        &self.output,
                        "running",
                        &container.id,
                        &remote_user,
                        workspace_folder_str,
                    );
                    return Ok(());
                }
                (ContainerState::Running, true) => {
                    // Deregister from daemon before stopping (like down.rs)
                    if let Some(mgmt_sock) =
                        cella_env::git_credential::daemon_management_socket_path()
                        && mgmt_sock.exists()
                    {
                        let req = cella_port::protocol::ManagementRequest::DeregisterContainer {
                            container_name: container_nm.clone(),
                        };
                        let _ = cella_daemon::management::send_management_request(&mgmt_sock, &req)
                            .await;
                    }
                    info!("Stopping container for rebuild...");
                    client.stop_container(&container.id).await?;
                    client.remove_container(&container.id, false).await?;
                }
                (ContainerState::Stopped, false) => {
                    super::ensure_cella_daemon().await;

                    // Warn if config has changed
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

                    // Warn if Docker runtime has changed
                    let current_runtime = cella_env::platform::detect_runtime();
                    if let Some(old_runtime) = container.labels.get("dev.cella.docker_runtime") {
                        let current_label = current_runtime.as_label();
                        if old_runtime != current_label {
                            eprintln!(
                                "\x1b[33mWARNING:\x1b[0m Docker runtime changed ({old_runtime} \u{2192} {current_label})."
                            );
                            eprintln!(
                                "  Run `cella up --rebuild` to recreate with the updated runtime."
                            );
                        }
                    }

                    // Attempt to start directly — let Docker validate mounts
                    let start_result = timed_step(
                        is_text,
                        "Starting container...",
                        client.start_container(&container.id),
                    )
                    .await;

                    match start_result {
                        Ok(()) => {
                            verify_container_running(&client, &container.id).await?;

                            // Re-inject env forwarding on restart
                            let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user);
                            timed_step(
                                is_text,
                                "Configuring environment...",
                                inject_post_start(
                                    &client,
                                    &container.id,
                                    &env_fwd.post_start,
                                    &remote_user,
                                ),
                            )
                            .await;

                            // Claude Code: re-sync auth + ensure installed on restart
                            let cc_settings =
                                cella_config::CellaSettings::load(&resolved.workspace_root);
                            if cc_settings.tools.claude_code.forward_config {
                                resync_claude_auth(&client, &container.id, &remote_user).await;
                            }
                            if cc_settings.tools.claude_code.enabled {
                                install_claude_code(
                                    &client,
                                    &container.id,
                                    &remote_user,
                                    &cc_settings.tools.claude_code,
                                )
                                .await;
                            }

                            timed_step(
                                is_text,
                                "Running userEnvProbe...",
                                super::env_cache::probe_and_cache_user_env(
                                    &client,
                                    &container.id,
                                    &remote_user,
                                    probe_type,
                                ),
                            )
                            .await;

                            // Run lifecycle from metadata label (includes features)
                            let metadata = container.labels.get("devcontainer.metadata");
                            for phase in ["postStartCommand", "postAttachCommand"] {
                                let entries = metadata.map_or_else(
                                    || {
                                        config
                                            .get(phase)
                                            .filter(|v| !v.is_null())
                                            .map(|cmd| {
                                                vec![cella_features::LifecycleEntry {
                                                    origin: "devcontainer.json".into(),
                                                    command: cmd.clone(),
                                                }]
                                            })
                                            .unwrap_or_default()
                                    },
                                    |meta_json| {
                                        cella_features::lifecycle_from_metadata_label(
                                            meta_json, phase,
                                        )
                                    },
                                );
                                run_lifecycle_entries(
                                    &client,
                                    &container.id,
                                    phase,
                                    &entries,
                                    Some(remote_user.as_str()),
                                    &remote_env,
                                    workspace_folder,
                                    is_text,
                                )
                                .await?;
                            }

                            output_result(
                                &self.output,
                                "started",
                                &container.id,
                                &remote_user,
                                workspace_folder_str,
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            warn!("Failed to start existing container: {e}");
                            eprintln!(
                                "\x1b[33mWARNING:\x1b[0m Could not start existing container: {e}"
                            );
                            eprintln!("Recreating container...");
                            let _ = client.remove_container(&container.id, false).await;
                            // Fall through to creation path
                        }
                    }
                }
                (ContainerState::Running, false) => {
                    // build_no_cache=true with running container: stop, remove, rebuild
                    info!("Stopping container for --build-no-cache...");
                    client.stop_container(&container.id).await?;
                    client.remove_container(&container.id, false).await?;
                }
                (_, true) => {
                    // Rebuild: stop if running, then remove
                    if container.state == ContainerState::Running {
                        client.stop_container(&container.id).await?;
                    }
                    client.remove_container(&container.id, false).await?;
                }
                (ContainerState::Created, false) => {
                    // Created but never started — remove and recreate
                    client.remove_container(&container.id, false).await?;
                }
                (_, false) => {
                    // Other state (Removing, etc.) — remove and recreate
                    client.remove_container(&container.id, false).await?;
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
            is_text,
        )
        .await?;

        // 6. Inspect image env for merging with user containerEnv
        let image_env = client.inspect_image_env(&img_name).await?;

        // 6.1. Resolve remote_user: remoteUser > containerUser > image USER
        let remote_user = if let Some(u) = config.get("remoteUser").and_then(|v| v.as_str()) {
            u.to_string()
        } else if let Some(u) = config.get("containerUser").and_then(|v| v.as_str()) {
            u.to_string()
        } else {
            client.inspect_image_user(&img_name).await?
        };

        // 6.5. Ensure credential proxy daemon is running (if host has git credentials)
        super::ensure_cella_daemon().await;

        // 6.6. Prepare environment forwarding (SSH agent, git config, credential proxy)
        let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user);

        // 7. Create container
        let docker_runtime = cella_env::platform::detect_runtime();
        let mut labels = container_labels(
            &resolved.workspace_root,
            &resolved.config_path,
            &resolved.config_hash,
            docker_runtime.as_label(),
        );

        // Store exec metadata labels for fast exec/shell without config re-resolution
        labels.insert("dev.cella.remote_user".to_string(), remote_user.clone());
        labels.insert(
            "dev.cella.workspace_folder".to_string(),
            workspace_folder_str.to_string(),
        );
        // Merge forwarding env (SSH_AUTH_SOCK, CELLA_CREDENTIAL_SOCKET, etc.) into the
        // label so exec/shell can pick them up without re-resolving config.
        let mut label_remote_env = remote_env.clone();
        for e in &env_fwd.env {
            label_remote_env.push(format!("{}={}", e.key, e.value));
        }
        if !label_remote_env.is_empty() {
            labels.insert(
                "dev.cella.remote_env".to_string(),
                serde_json::to_string(&label_remote_env).unwrap_or_default(),
            );
        }

        if let Some(ref rf) = resolved_features {
            labels.insert(
                "devcontainer.metadata".to_string(),
                rf.metadata_label.clone(),
            );
        }

        // Store ports_attributes in a label for re-registration after daemon restart
        {
            let ports_attrs = cella_docker::config_map::ports::parse_ports_attributes(config);
            let other_ports_attrs =
                cella_docker::config_map::ports::parse_other_ports_attributes(config);
            labels.insert(
                "dev.cella.ports_attributes".to_string(),
                cella_docker::config_map::ports::serialize_ports_attributes_label(
                    &ports_attrs,
                    other_ports_attrs.as_ref(),
                ),
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

        // 7.1. Auto-mount ~/.claude.json if Claude Code config forwarding is enabled
        let settings = cella_config::CellaSettings::load(&resolved.workspace_root);
        if settings.tools.claude_code.forward_config
            && let Some(host_path) = cella_env::claude_code::host_claude_json_path()
        {
            let target = format!(
                "{}/.claude.json",
                cella_env::claude_code::container_home(&remote_user),
            );
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
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

        // Query daemon for control port + token to inject as env vars
        let daemon_env = if let Some(mgmt_sock) =
            cella_env::git_credential::daemon_management_socket_path()
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
                vec![
                    format!("CELLA_DAEMON_ADDR=host.docker.internal:{control_port}"),
                    format!("CELLA_DAEMON_TOKEN={control_token}"),
                    format!("CELLA_CONTAINER_NAME={container_nm}"),
                ]
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        if !daemon_env.is_empty() {
            if create_opts.env.is_empty() {
                create_opts.env = image_env.clone();
            }
            create_opts.env.extend(daemon_env);
        }

        // Populate agent volume with binary + browser helper
        if let Err(e) = cella_docker::volume::ensure_agent_volume_populated(client.inner()).await {
            warn!("Failed to populate agent volume: {e}");
            eprintln!(
                "\x1b[33mWARNING:\x1b[0m Port forwarding and BROWSER interception will not work."
            );
            eprintln!("  Agent volume population failed: {e}");
        }

        // Add agent volume mount and env vars
        {
            let agent_env = cella_docker::config_map::env::agent_env_vars();
            if create_opts.env.is_empty() {
                create_opts.env = image_env.clone();
            }
            create_opts.env.extend(agent_env);

            // Add cella-agent volume mount (read-only)
            let (vol_name, vol_target, _ro) = cella_docker::volume::agent_volume_mount();
            create_opts.mounts.push(MountConfig {
                mount_type: "volume".to_string(),
                source: vol_name.to_string(),
                target: vol_target.to_string(),
                consistency: None,
            });
        }

        let container_id = timed_step(
            is_text,
            "Creating container...",
            client.create_container(&create_opts),
        )
        .await?;

        // 8. Start container
        timed_step(
            is_text,
            "Starting container...",
            client.start_container(&container_id),
        )
        .await?;
        verify_container_running(&client, &container_id).await?;

        // 8.5. Connect container to cella bridge network (for cross-container comm)
        if let Err(e) =
            cella_docker::network::ensure_container_connected(client.inner(), &container_id).await
        {
            tracing::warn!("Failed to connect container to cella network: {e}");
        }

        // 8.6. Connect to per-repository network (enables inter-container DNS)
        if let Err(e) = cella_docker::network::ensure_repo_network(
            client.inner(),
            &container_id,
            &resolved.workspace_root,
        )
        .await
        {
            tracing::warn!("Failed to connect container to repo network: {e}");
        }

        // 8.7. Register container with daemon for port management
        {
            let container_ip =
                cella_docker::network::get_container_cella_ip(client.inner(), &container_id).await;
            if let Some(mgmt_sock) = cella_env::git_credential::daemon_management_socket_path()
                && mgmt_sock.exists()
            {
                // Parse forwardPorts from devcontainer.json
                let forward_ports: Vec<u16> = config
                    .get("forwardPorts")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                            .collect()
                    })
                    .unwrap_or_default();

                let ports_attrs = cella_docker::config_map::ports::parse_ports_attributes(config);
                let other_ports_attrs =
                    cella_docker::config_map::ports::parse_other_ports_attributes(config);
                let req = cella_port::protocol::ManagementRequest::RegisterContainer {
                    container_id: container_id.clone(),
                    container_name: container_nm.clone(),
                    container_ip,
                    ports_attributes: ports_attrs,
                    other_ports_attributes: other_ports_attrs,
                    forward_ports,
                };
                match cella_daemon::management::send_management_request(&mgmt_sock, &req).await {
                    Ok(resp) => {
                        tracing::debug!("Container registered with daemon: {resp:?}");
                    }
                    Err(e) => {
                        warn!("Failed to register container with daemon: {e}");
                    }
                }
            }
        }

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
                &remote_user,
                &resolved.workspace_root,
            )
            .await
        {
            warn!("Failed to update remote user UID: {e}");
        }

        // 9.5. Inject post-start environment forwarding (SSH files, git config, credential helper)
        timed_step(
            is_text,
            "Configuring environment...",
            inject_post_start(&client, &container_id, &env_fwd.post_start, &remote_user),
        )
        .await;

        // 9.7. Seed gh CLI credentials (first create only)
        if settings.credentials.gh {
            seed_gh_credentials(
                &client,
                &container_id,
                &resolved.workspace_root,
                &remote_user,
            )
            .await;
        }

        // 9.8. Claude Code: forward config + install (first create)
        if settings.tools.claude_code.forward_config {
            timed_step(
                is_text,
                "Forwarding Claude Code config...",
                seed_claude_config(
                    &client,
                    &container_id,
                    &resolved.workspace_root,
                    &remote_user,
                    &settings.tools.claude_code,
                ),
            )
            .await;
        }
        if settings.tools.claude_code.enabled {
            timed_step(
                is_text,
                "Installing Claude Code...",
                install_claude_code(
                    &client,
                    &container_id,
                    &remote_user,
                    &settings.tools.claude_code,
                ),
            )
            .await;
        }

        // 9.6. Probe and cache user environment (userEnvProbe)
        let probe_type = config
            .get("userEnvProbe")
            .and_then(|v| v.as_str())
            .unwrap_or("loginInteractiveShell");

        timed_step(
            is_text,
            "Running userEnvProbe...",
            super::env_cache::probe_and_cache_user_env(
                &client,
                &container_id,
                &remote_user,
                probe_type,
            ),
        )
        .await;

        // 10-14. Lifecycle commands (first create)
        let lifecycle_phases = [
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ];

        for phase in lifecycle_phases {
            // Merged lifecycle entries include both feature and user commands with
            // origins.  updateContentCommand is not part of feature lifecycle, so
            // falls through to the config-only branch.
            let empty = Vec::new();
            let entries = resolved_features.as_ref().map_or(&empty, |rf| match phase {
                "onCreateCommand" => &rf.container_config.lifecycle.on_create,
                "postCreateCommand" => &rf.container_config.lifecycle.post_create,
                "postStartCommand" => &rf.container_config.lifecycle.post_start,
                "postAttachCommand" => &rf.container_config.lifecycle.post_attach,
                _ => &empty,
            });

            run_lifecycle_entries(
                &client,
                &container_id,
                phase,
                entries,
                Some(remote_user.as_str()),
                &create_opts.remote_env,
                workspace_folder,
                is_text,
            )
            .await?;

            // For phases not in the merged lifecycle (updateContentCommand,
            // or when no features are resolved), run user commands directly.
            if entries.is_empty()
                && let Some(cmd) = config.get(phase)
                && !cmd.is_null()
            {
                lifecycle::run_lifecycle_phase(
                    &client,
                    &container_id,
                    phase,
                    cmd,
                    "devcontainer.json",
                    Some(remote_user.as_str()),
                    &create_opts.remote_env,
                    workspace_folder,
                    is_text,
                )
                .await?;
            }
        }

        // 15. Output
        output_result(
            &self.output,
            "created",
            &container_id,
            &remote_user,
            workspace_folder_str,
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

/// Print a progress label, run an async operation, and optionally show elapsed time.
async fn timed_step<F, T>(is_text: bool, label: &str, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    if is_text {
        eprint!("{label}");
    }
    let start = std::time::Instant::now();
    let result = f.await;
    if is_text {
        let elapsed = start.elapsed();
        if elapsed.as_millis() >= 100 {
            eprintln!(" ({:.1}s)", elapsed.as_secs_f64());
        } else {
            eprintln!();
        }
    }
    result
}

/// Run a sequence of origin-tracked lifecycle entries.
#[allow(clippy::too_many_arguments)]
async fn run_lifecycle_entries(
    client: &DockerClient,
    container_id: &str,
    phase: &str,
    entries: &[cella_features::LifecycleEntry],
    user: Option<&str>,
    env: &[String],
    working_dir: Option<&str>,
    is_text: bool,
) -> Result<(), CellaDockerError> {
    for entry in entries {
        lifecycle::run_lifecycle_phase(
            client,
            container_id,
            phase,
            &entry.command,
            &entry.origin,
            user,
            env,
            working_dir,
            is_text,
        )
        .await?;
    }
    Ok(())
}

/// Seed gh CLI credentials into a container.
///
/// Extracts tokens from the host's gh CLI and uploads `hosts.yml` and `config.yml`
/// into the container. Skips silently if gh is not installed/authenticated or if
/// credentials already exist in the container.
async fn seed_gh_credentials(
    client: &DockerClient,
    container_id: &str,
    workspace_root: &std::path::Path,
    remote_user: &str,
) {
    let config_dir = cella_env::gh_credential::gh_config_dir_for_user(remote_user);

    // Check if gh credentials already exist in container
    let check_cmd = cella_env::gh_credential::gh_config_exists_in_container(&config_dir);
    if client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: check_cmd,
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0)
    {
        tracing::debug!("gh credentials already present in container, skipping seed");
        return;
    }

    // Prepare credentials from host
    let Some(gh_creds) =
        cella_env::gh_credential::prepare_gh_credentials(workspace_root, remote_user)
    else {
        return;
    };

    // Create the config directory
    if let Err(e) = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("mkdir -p {config_dir} && chmod 700 {config_dir}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        warn!("Failed to create gh config directory: {e}");
        return;
    }

    // Upload credential files
    let docker_files: Vec<FileToUpload> = gh_creds
        .file_uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect();

    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        warn!("Failed to upload gh credential files: {e}");
        return;
    }

    // Fix ownership
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "chown".to_string(),
                    "-R".to_string(),
                    format!("{remote_user}:{remote_user}"),
                    config_dir.clone(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    info!("Seeded gh CLI credentials into container");
}

/// Seed Claude Code config files into a container (first create).
///
/// Copies `~/.claude/` from host with path rewriting, then fixes ownership.
/// Skips silently if `~/.claude/` doesn't exist on host or config already
/// exists in the container.
async fn seed_claude_config(
    client: &DockerClient,
    container_id: &str,
    workspace_root: &std::path::Path,
    remote_user: &str,
    settings: &cella_config::ClaudeCodeSettings,
) {
    let claude_dir = cella_env::claude_code::claude_dir_for_user(remote_user);

    // Check if Claude config already exists in container
    let check_cmd = cella_env::claude_code::claude_config_exists_command(&claude_dir);
    if client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: check_cmd,
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0)
    {
        tracing::debug!("Claude config already present in container, skipping seed");
        return;
    }

    // Prepare config files from host with path rewriting
    let Some(uploads) =
        cella_env::claude_code::prepare_claude_config(remote_user, workspace_root, settings)
    else {
        return;
    };

    // Create the target directory
    if let Err(e) = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("mkdir -p {claude_dir}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        warn!("Failed to create .claude directory: {e}");
        return;
    }

    // Upload files
    let docker_files: Vec<FileToUpload> = uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect();

    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        warn!("Failed to upload Claude config files: {e}");
        return;
    }

    // Fix ownership
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "chown".to_string(),
                    "-R".to_string(),
                    format!("{remote_user}:{remote_user}"),
                    claude_dir,
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    info!("Seeded Claude Code config into container");
}

/// Re-sync Claude Code auth credentials on container restart.
///
/// Only re-uploads `.credentials.json` — does not overwrite other config.
async fn resync_claude_auth(client: &DockerClient, container_id: &str, remote_user: &str) {
    let Some(uploads) = cella_env::claude_code::prepare_claude_auth_resync(remote_user) else {
        return;
    };

    let docker_files: Vec<FileToUpload> = uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect();

    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        tracing::debug!("Failed to re-sync Claude auth: {e}");
    }
}

/// Install Claude Code inside the container using the native installer.
///
/// Checks if already installed at the correct version first.
/// On Alpine/musl, pre-installs required dependencies.
/// Never fails `cella up` — logs errors and continues.
async fn install_claude_code(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::ClaudeCodeSettings,
) {
    // Check if already installed at correct version
    let version_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["claude".to_string(), "--version".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    if let Ok(result) = &version_check
        && result.exit_code == 0
    {
        let installed_version = result.stdout.trim();
        if settings.version == "latest" || settings.version == "stable" {
            tracing::debug!("Claude Code already installed: {installed_version}");
            return;
        }
        if installed_version.contains(&settings.version) {
            tracing::debug!(
                "Claude Code already at version {}: {installed_version}",
                settings.version
            );
            return;
        }
    }

    // Detect Alpine/musl and install dependencies
    let is_alpine = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "test".to_string(),
                    "-f".to_string(),
                    "/etc/alpine-release".to_string(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0);

    if is_alpine {
        info!("Alpine detected, installing Claude Code dependencies...");
        let _ = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "apk add --no-cache libgcc libstdc++ ripgrep".to_string(),
                    ],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
    }

    // Warn if version is pinned with native installer
    if settings.version != "latest" && settings.version != "stable" {
        info!(
            "Installing Claude Code v{} (native installer will attempt version pinning)",
            settings.version
        );
    }

    // Install via native installer
    let install_cmd = format!(
        "curl -fsSL https://claude.ai/install.sh | bash -s {}",
        settings.version
    );

    info!("Installing Claude Code ({})...", settings.version);
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_cmd],
                user: Some(remote_user.to_string()),
                env: if is_alpine {
                    Some(vec!["USE_BUILTIN_RIPGREP=0".to_string()])
                } else {
                    None
                },
                working_dir: None,
            },
        )
        .await;

    match result {
        Ok(r) if r.exit_code == 0 => {
            info!("Claude Code installed successfully");
        }
        Ok(r) => {
            warn!(
                "Claude Code installation exited with code {}: {}",
                r.exit_code,
                r.stderr.trim()
            );
        }
        Err(e) => {
            warn!("Claude Code installation failed: {e}");
        }
    }
}
