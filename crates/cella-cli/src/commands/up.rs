use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::resolve::{self, ResolvedConfig};
use cella_docker::{
    CellaDockerError, ContainerInfo, ContainerState, DockerClient, ExecOptions, FileToUpload,
    LifecycleContext, MountConfig, container_labels, container_name, run_lifecycle_phase,
    update_remote_user_uid,
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

/// Holds resolved state for an `up` invocation, shared across all code paths.
struct UpContext {
    resolved: ResolvedConfig,
    client: DockerClient,
    container_nm: String,
    remote_env: Vec<String>,
    workspace_folder_from_config: Option<String>,
    default_workspace_folder: String,
    is_text: bool,
    output: OutputFormat,
    remove_container: bool,
    build_no_cache: bool,
}

impl UpContext {
    async fn new(args: &UpArgs) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = if let Some(ref wf) = args.workspace_folder {
            wf.canonicalize().unwrap_or_else(|_| wf.clone())
        } else {
            std::env::current_dir()?
        };

        let remove_container = args.rebuild || args.remove_existing_container;
        let is_text = matches!(&args.output, OutputFormat::Text);

        // 1. Resolve config
        if is_text {
            eprintln!("Resolving devcontainer configuration...");
        }
        let resolved = resolve::config(&cwd, args.file.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        // 2. Connect to Docker
        let client = match &args.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };
        client.ping().await?;

        let container_nm = container_name(&resolved.workspace_root, config_name);
        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder_from_config = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map(String::from);
        let workspace_basename = resolved.workspace_root.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let default_workspace_folder = format!("/workspaces/{workspace_basename}");

        Ok(Self {
            resolved,
            client,
            container_nm,
            remote_env,
            workspace_folder_from_config,
            default_workspace_folder,
            is_text,
            output: args.output.clone(),
            remove_container,
            build_no_cache: args.build_no_cache,
        })
    }

    const fn config(&self) -> &serde_json::Value {
        &self.resolved.config
    }

    fn config_name(&self) -> Option<&str> {
        self.config().get("name").and_then(|v| v.as_str())
    }

    fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder_from_config.as_deref()
    }

    fn workspace_folder_str(&self) -> &str {
        self.workspace_folder_from_config
            .as_deref()
            .unwrap_or(&self.default_workspace_folder)
    }

    fn probe_type(&self) -> &str {
        self.config()
            .get("userEnvProbe")
            .and_then(|v| v.as_str())
            .unwrap_or("loginInteractiveShell")
    }

    /// Resolve `remote_user` from an existing container's labels and image metadata.
    ///
    /// Priority: `remoteUser` (config) > `containerUser` (config) > `remoteUser` (image metadata)
    ///         > `containerUser` (image metadata) > image USER > label fallback > `"root"`
    async fn resolve_remote_user_from_container(&self, container: &ContainerInfo) -> String {
        let config = self.config();
        if let Some(u) = config.get("remoteUser").and_then(|v| v.as_str()) {
            return u.to_string();
        }
        if let Some(u) = config.get("containerUser").and_then(|v| v.as_str()) {
            return u.to_string();
        }
        // Check image metadata from stored container label
        let meta_user = container
            .labels
            .get("devcontainer.metadata")
            .map(|m| cella_features::parse_image_metadata(m).1);
        if let Some(u) = meta_user.as_ref().and_then(|m| m.remote_user.as_deref()) {
            return u.to_string();
        }
        if let Some(u) = meta_user.as_ref().and_then(|m| m.container_user.as_deref()) {
            return u.to_string();
        }
        if let Some(ref img) = container.image {
            return self
                .client
                .inspect_image_user(img)
                .await
                .unwrap_or_else(|_| "root".to_string());
        }
        container
            .labels
            .get("dev.cella.remote_user")
            .cloned()
            .unwrap_or_else(|| "root".to_string())
    }

    /// Run the env forwarding + userEnvProbe + tool auth/install sequence
    /// that is shared between the running and stopped-restart paths.
    async fn prepare_container_env(
        &self,
        container_id: &str,
        remote_user: &str,
    ) -> (
        Option<std::collections::HashMap<String, String>>,
        Vec<String>,
    ) {
        let config = self.config();

        // Re-inject env forwarding (git config + SSH files may have changed)
        let env_fwd = cella_env::prepare_env_forwarding(config, remote_user);
        timed_step(
            self.is_text,
            "Configuring environment...",
            inject_post_start(&self.client, container_id, &env_fwd.post_start, remote_user),
        )
        .await;

        // Probe user environment first so tool installs can use feature-provided PATH
        // (e.g., nvm adds /usr/local/share/nvm/current/bin via login shell profiles)
        let probed_env = timed_step(
            self.is_text,
            "Running userEnvProbe...",
            super::env_cache::probe_and_cache_user_env(
                &self.client,
                container_id,
                remote_user,
                self.probe_type(),
            ),
        )
        .await;

        // Claude Code: re-sync auth + ensure installed
        let settings = cella_config::Settings::load(&self.resolved.workspace_root);
        if settings.tools.claude_code.forward_config {
            resync_claude_auth(&self.client, container_id, remote_user).await;
        }
        if settings.tools.claude_code.enabled {
            install_claude_code(
                &self.client,
                container_id,
                remote_user,
                &settings.tools.claude_code,
                probed_env.as_ref(),
            )
            .await;
        }

        // Codex/Gemini: no auth resync needed (bind mount), just ensure installed
        if settings.tools.codex.enabled {
            install_codex(
                &self.client,
                container_id,
                remote_user,
                &settings.tools.codex,
                probed_env.as_ref(),
            )
            .await;
        }
        if settings.tools.gemini.enabled {
            install_gemini(
                &self.client,
                container_id,
                remote_user,
                &settings.tools.gemini,
                probed_env.as_ref(),
            )
            .await;
        }

        let lifecycle_env = probed_env.as_ref().map_or_else(
            || self.remote_env.clone(),
            |probed| cella_env::user_env_probe::merge_env(probed, &self.remote_env),
        );

        (probed_env, lifecycle_env)
    }

    /// Handle an already-running container (no rebuild requested, no `--build-no-cache`).
    async fn handle_running(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        super::ensure_cella_daemon().await;

        let (_probed_env, lifecycle_env) =
            self.prepare_container_env(&container.id, remote_user).await;

        // Already running -- run postAttachCommand from metadata (includes features)
        let metadata = container.labels.get("devcontainer.metadata");
        let entries = lifecycle_entries_for_phase(
            metadata.map(String::as_str),
            self.config(),
            "postAttachCommand",
        );
        let lc_ctx = LifecycleContext {
            client: &self.client,
            container_id: &container.id,
            user: Some(remote_user),
            env: &lifecycle_env,
            working_dir: self.workspace_folder(),
            is_text: self.is_text,
        };
        run_lifecycle_entries(&lc_ctx, "postAttachCommand", &entries).await?;

        output_result(
            &self.output,
            "running",
            &container.id,
            remote_user,
            self.workspace_folder_str(),
        );
        Ok(())
    }

    /// Handle a stopped container: try to restart it.
    ///
    /// Returns `Ok(true)` if the container was successfully restarted (caller should return),
    /// `Ok(false)` if restart failed and the container was removed (caller should fall through
    /// to the create path).
    async fn handle_stopped(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        super::ensure_cella_daemon().await;

        // Warn if config has changed
        if let Some(old_hash) = &container.config_hash
            && *old_hash != self.resolved.config_hash
        {
            eprintln!(
                "\x1b[33mWARNING:\x1b[0m Config has changed since this container was created."
            );
            eprintln!("  Run `cella up --rebuild` to recreate with the updated config.");
        }

        // Warn if Docker runtime has changed
        let current_runtime = cella_env::platform::detect_runtime();
        if let Some(old_runtime) = container.labels.get("dev.cella.docker_runtime") {
            let current_label = current_runtime.as_label();
            if old_runtime != current_label {
                eprintln!(
                    "\x1b[33mWARNING:\x1b[0m Docker runtime changed ({old_runtime} \u{2192} {current_label})."
                );
                eprintln!("  Run `cella up --rebuild` to recreate with the updated runtime.");
            }
        }

        // Attempt to start directly -- let Docker validate mounts
        let start_result = timed_step(
            self.is_text,
            "Starting container...",
            self.client.start_container(&container.id),
        )
        .await;

        match start_result {
            Ok(()) => {
                verify_container_running(&self.client, &container.id).await?;

                let (_probed_env, lifecycle_env) =
                    self.prepare_container_env(&container.id, remote_user).await;

                // Run lifecycle from metadata label (includes features)
                let metadata = container.labels.get("devcontainer.metadata");
                for phase in ["postStartCommand", "postAttachCommand"] {
                    let entries = lifecycle_entries_for_phase(
                        metadata.map(String::as_str),
                        self.config(),
                        phase,
                    );
                    let lc_ctx = LifecycleContext {
                        client: &self.client,
                        container_id: &container.id,
                        user: Some(remote_user),
                        env: &lifecycle_env,
                        working_dir: self.workspace_folder(),
                        is_text: self.is_text,
                    };
                    run_lifecycle_entries(&lc_ctx, phase, &entries).await?;
                }

                output_result(
                    &self.output,
                    "started",
                    &container.id,
                    remote_user,
                    self.workspace_folder_str(),
                );
                Ok(true)
            }
            Err(e) => {
                warn!("Failed to start existing container: {e}");
                eprintln!("\x1b[33mWARNING:\x1b[0m Could not start existing container: {e}");
                eprintln!("Recreating container...");
                let _ = self.client.remove_container(&container.id, false).await;
                // Fall through to creation path
                Ok(false)
            }
        }
    }

    /// Deregister, stop, and remove an existing container.
    async fn remove_existing(
        &self,
        container: &ContainerInfo,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Deregister from daemon before stopping (like down.rs)
        if container.state == ContainerState::Running {
            if let Some(mgmt_sock) = cella_env::git_credential::daemon_management_socket_path()
                && mgmt_sock.exists()
            {
                let req = cella_port::protocol::ManagementRequest::DeregisterContainer {
                    container_name: self.container_nm.clone(),
                };
                let _ = cella_daemon::management::send_management_request(&mgmt_sock, &req).await;
            }
            info!("Stopping container for {reason}...");
            self.client.stop_container(&container.id).await?;
        }
        self.client.remove_container(&container.id, false).await?;
        Ok(())
    }

    /// Build container labels from resolved config, features, and image metadata.
    fn build_labels(
        &self,
        resolved_features: Option<&cella_features::ResolvedFeatures>,
        base_metadata: Option<&str>,
        env_fwd: &cella_env::EnvForwarding,
        remote_user: &str,
    ) -> std::collections::HashMap<String, String> {
        let config = self.config();
        let docker_runtime = cella_env::platform::detect_runtime();
        let mut labels = container_labels(
            &self.resolved.workspace_root,
            &self.resolved.config_path,
            &self.resolved.config_hash,
            docker_runtime.as_label(),
        );

        labels.insert("dev.cella.remote_user".to_string(), remote_user.to_string());
        labels.insert(
            "dev.cella.workspace_folder".to_string(),
            self.workspace_folder_str().to_string(),
        );

        let mut label_remote_env = self.remote_env.clone();
        for e in &env_fwd.env {
            label_remote_env.push(format!("{}={}", e.key, e.value));
        }
        if !label_remote_env.is_empty() {
            labels.insert(
                "dev.cella.remote_env".to_string(),
                serde_json::to_string(&label_remote_env).unwrap_or_default(),
            );
        }

        if let Some(rf) = resolved_features {
            labels.insert(
                "devcontainer.metadata".to_string(),
                rf.metadata_label.clone(),
            );
        } else if base_metadata.is_some() {
            labels.insert(
                "devcontainer.metadata".to_string(),
                cella_features::generate_metadata_label(&[], config, base_metadata),
            );
        }

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

        labels
    }

    /// Merge forwarding mounts, env vars, daemon env, and agent volume into create options.
    async fn apply_env_and_mounts(
        &self,
        create_opts: &mut cella_docker::CreateContainerOptions,
        env_fwd: &cella_env::EnvForwarding,
        image_env: &[String],
        remote_user: &str,
        settings: &cella_config::Settings,
    ) {
        // Forwarding mounts
        for m in &env_fwd.mounts {
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: m.source.clone(),
                target: m.target.clone(),
                consistency: None,
            });
        }

        // Auto-mount ~/.claude.json if Claude Code config forwarding is enabled
        if settings.tools.claude_code.forward_config
            && let Some(host_path) = cella_env::claude_code::host_claude_json_path()
        {
            let target = format!(
                "{}/.claude.json",
                cella_env::claude_code::container_home(remote_user),
            );
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
                consistency: None,
            });
        }

        // Auto-mount ~/.codex if Codex config forwarding is enabled
        if settings.tools.codex.forward_config
            && let Some(host_path) = cella_env::codex::host_codex_dir()
        {
            let target = cella_env::codex::container_codex_dir(remote_user);
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
                consistency: None,
            });
        }

        // Auto-mount ~/.gemini if Gemini config forwarding is enabled
        if settings.tools.gemini.forward_config
            && let Some(host_path) = cella_env::gemini::host_gemini_dir()
        {
            let target = cella_env::gemini::container_gemini_dir(remote_user);
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
                consistency: None,
            });
        }

        // Forwarding env vars
        if !env_fwd.env.is_empty() {
            let fwd_env: Vec<String> = env_fwd
                .env
                .iter()
                .map(|e| format!("{}={}", e.key, e.value))
                .collect();
            if create_opts.env.is_empty() {
                create_opts.env = image_env.to_vec();
            }
            create_opts.env.extend(fwd_env);
        }

        // Daemon control port + token env vars
        let daemon_env = query_daemon_env(&self.container_nm).await;
        if !daemon_env.is_empty() {
            if create_opts.env.is_empty() {
                create_opts.env = image_env.to_vec();
            }
            create_opts.env.extend(daemon_env);
        }

        // Agent volume mount and env vars
        if let Err(e) =
            cella_docker::volume::ensure_agent_volume_populated(self.client.inner()).await
        {
            warn!("Failed to populate agent volume: {e}");
            eprintln!(
                "\x1b[33mWARNING:\x1b[0m Port forwarding and BROWSER interception will not work."
            );
            eprintln!("  Agent volume population failed: {e}");
        }

        let agent_env = cella_docker::config_map::env::agent_env_vars();
        if create_opts.env.is_empty() {
            create_opts.env = image_env.to_vec();
        }
        create_opts.env.extend(agent_env);

        let (vol_name, vol_target, _ro) = cella_docker::volume::agent_volume_mount();
        create_opts.mounts.push(MountConfig {
            mount_type: "volume".to_string(),
            source: vol_name.to_string(),
            target: vol_target.to_string(),
            consistency: None,
        });
    }

    /// Start the container, connect networks, and register with the daemon.
    async fn start_and_register(
        &self,
        container_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        timed_step(
            self.is_text,
            "Starting container...",
            self.client.start_container(container_id),
        )
        .await?;
        verify_container_running(&self.client, container_id).await?;

        if let Err(e) =
            cella_docker::network::ensure_container_connected(self.client.inner(), container_id)
                .await
        {
            tracing::warn!("Failed to connect container to cella network: {e}");
        }

        if let Err(e) = cella_docker::network::ensure_repo_network(
            self.client.inner(),
            container_id,
            &self.resolved.workspace_root,
        )
        .await
        {
            tracing::warn!("Failed to connect container to repo network: {e}");
        }

        self.register_with_daemon(container_id).await;
        Ok(())
    }

    /// Register the container with the daemon for port management.
    async fn register_with_daemon(&self, container_id: &str) {
        let config = self.config();
        let container_ip =
            cella_docker::network::get_container_cella_ip(self.client.inner(), container_id).await;

        let Some(mgmt_sock) = cella_env::git_credential::daemon_management_socket_path() else {
            return;
        };
        if !mgmt_sock.exists() {
            return;
        }

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
            container_id: container_id.to_string(),
            container_name: self.container_nm.clone(),
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

    /// Run post-create setup: UID update, env injection, credentials, Claude Code, userEnvProbe.
    async fn post_create_setup(
        &self,
        container_id: &str,
        remote_user: &str,
        env_fwd: &cella_env::EnvForwarding,
        settings: &cella_config::Settings,
        remote_env: &[String],
    ) -> (
        Option<std::collections::HashMap<String, String>>,
        Vec<String>,
    ) {
        let config = self.config();

        // updateRemoteUserUID
        let update_uid = config
            .get("updateRemoteUserUID")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        if update_uid
            && remote_user != "root"
            && let Err(e) = update_remote_user_uid(
                &self.client,
                container_id,
                remote_user,
                &self.resolved.workspace_root,
            )
            .await
        {
            warn!("Failed to update remote user UID: {e}");
        }

        // Inject post-start environment forwarding
        timed_step(
            self.is_text,
            "Configuring environment...",
            inject_post_start(&self.client, container_id, &env_fwd.post_start, remote_user),
        )
        .await;

        // Seed gh CLI credentials (first create only)
        if settings.credentials.gh {
            seed_gh_credentials(
                &self.client,
                container_id,
                &self.resolved.workspace_root,
                remote_user,
            )
            .await;
        }

        // Probe user environment first so tool installs can use feature-provided PATH
        // (e.g., nvm adds /usr/local/share/nvm/current/bin via login shell profiles)
        let probed_env = timed_step(
            self.is_text,
            "Running userEnvProbe...",
            super::env_cache::probe_and_cache_user_env(
                &self.client,
                container_id,
                remote_user,
                self.probe_type(),
            ),
        )
        .await;

        // Forward config and install AI coding tools
        self.install_tools(container_id, remote_user, settings, probed_env.as_ref())
            .await;

        let lifecycle_env = probed_env.as_ref().map_or_else(
            || remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, remote_env),
        );

        (probed_env, lifecycle_env)
    }

    /// Forward config and install AI coding tools (Claude Code, Codex, Gemini).
    async fn install_tools(
        &self,
        container_id: &str,
        remote_user: &str,
        settings: &cella_config::Settings,
        probed_env: Option<&std::collections::HashMap<String, String>>,
    ) {
        if settings.tools.claude_code.forward_config {
            timed_step(
                self.is_text,
                "Forwarding Claude Code config...",
                seed_claude_config(
                    &self.client,
                    container_id,
                    &self.resolved.workspace_root,
                    remote_user,
                    &settings.tools.claude_code,
                ),
            )
            .await;
        }
        if settings.tools.claude_code.enabled {
            timed_step(
                self.is_text,
                "Installing Claude Code...",
                install_claude_code(
                    &self.client,
                    container_id,
                    remote_user,
                    &settings.tools.claude_code,
                    probed_env,
                ),
            )
            .await;
        }
        if settings.tools.codex.enabled {
            timed_step(
                self.is_text,
                "Installing Codex...",
                install_codex(
                    &self.client,
                    container_id,
                    remote_user,
                    &settings.tools.codex,
                    probed_env,
                ),
            )
            .await;
        }
        if settings.tools.gemini.enabled {
            timed_step(
                self.is_text,
                "Installing Gemini CLI...",
                install_gemini(
                    &self.client,
                    container_id,
                    remote_user,
                    &settings.tools.gemini,
                    probed_env,
                ),
            )
            .await;
        }
    }

    /// The full build/create/start/lifecycle path for a new container.
    async fn create_and_start(
        &self,
        build_no_cache: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = self.config();

        // Run initializeCommand on host (runs every invocation per spec)
        if let Some(init_cmd) = config.get("initializeCommand") {
            run_host_command("initializeCommand", init_cmd)?;
        }

        // Ensure image (with optional features layer)
        let (img_name, resolved_features, base_image_details) = ensure_image(
            &self.client,
            config,
            &self.resolved.workspace_root,
            self.config_name(),
            &self.resolved.config_path,
            build_no_cache,
            self.is_text,
        )
        .await?;

        let image_env = base_image_details.env;
        let image_meta_user = base_image_details
            .metadata
            .as_deref()
            .map(|m| cella_features::parse_image_metadata(m).1);
        let remote_user =
            resolve_remote_user(config, image_meta_user.as_ref(), &base_image_details.user);

        super::ensure_cella_daemon().await;
        let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user);

        // Build labels and create options
        let labels = self.build_labels(
            resolved_features.as_ref(),
            base_image_details.metadata.as_deref(),
            &env_fwd,
            &remote_user,
        );

        let feature_config = resolved_features.as_ref().map(|r| &r.container_config);
        let image_meta_config = if feature_config.is_none() {
            base_image_details
                .metadata
                .as_deref()
                .map(|m| cella_features::parse_image_metadata(m).0)
        } else {
            None
        };
        let effective_feature_config = feature_config.or(image_meta_config.as_ref());

        let mut create_opts = cella_docker::config_map::map_config(
            config,
            &self.container_nm,
            &img_name,
            labels,
            &self.resolved.workspace_root,
            effective_feature_config,
            &image_env,
        );

        let settings = cella_config::Settings::load(&self.resolved.workspace_root);
        self.apply_env_and_mounts(
            &mut create_opts,
            &env_fwd,
            &image_env,
            &remote_user,
            &settings,
        )
        .await;

        // Create and start container
        let container_id = timed_step(
            self.is_text,
            "Creating container...",
            self.client.create_container(&create_opts),
        )
        .await?;

        self.start_and_register(&container_id).await?;

        // Post-create setup (UID, env, credentials, Claude Code, userEnvProbe)
        let (_probed_env, lifecycle_env) = self
            .post_create_setup(
                &container_id,
                &remote_user,
                &env_fwd,
                &settings,
                &create_opts.remote_env,
            )
            .await;

        // Lifecycle commands (first create)
        let lc_ctx = LifecycleContext {
            client: &self.client,
            container_id: &container_id,
            user: Some(remote_user.as_str()),
            env: &lifecycle_env,
            working_dir: self.workspace_folder(),
            is_text: self.is_text,
        };
        run_all_lifecycle_phases(&lc_ctx, config, resolved_features.as_ref()).await?;

        output_result(
            &self.output,
            "created",
            &container_id,
            &remote_user,
            self.workspace_folder_str(),
        );

        Ok(())
    }
}

impl UpArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let ctx = UpContext::new(&self).await?;
        let existing = ctx
            .client
            .find_container(&ctx.resolved.workspace_root)
            .await?;

        if let Some(container) = existing {
            let remote_user = ctx.resolve_remote_user_from_container(&container).await;

            match (&container.state, ctx.remove_container) {
                (ContainerState::Running, false) if !ctx.build_no_cache => {
                    return ctx.handle_running(&container, &remote_user).await;
                }
                (ContainerState::Stopped, false) => {
                    if ctx.handle_stopped(&container, &remote_user).await? {
                        return Ok(());
                    }
                    // Fall through to create_and_start
                }
                (ContainerState::Running, false) => {
                    // build_no_cache=true with running container: stop, remove, rebuild
                    ctx.remove_existing(&container, "--build-no-cache").await?;
                }
                (ContainerState::Running, true) => {
                    ctx.remove_existing(&container, "rebuild").await?;
                }
                (_, true) => {
                    // Rebuild: stop if running, then remove
                    if container.state == ContainerState::Running {
                        ctx.client.stop_container(&container.id).await?;
                    }
                    ctx.client.remove_container(&container.id, false).await?;
                }
                (_, false) => {
                    // Created but never started, or other state — remove and recreate
                    ctx.client.remove_container(&container.id, false).await?;
                }
            }
        }

        ctx.create_and_start(self.build_no_cache).await
    }
}

/// Resolve the remote user from config and image metadata.
///
/// Priority: `remoteUser` (config) > `containerUser` (config) > `remoteUser` (image metadata)
/// > `containerUser` (image metadata) > `fallback` (typically Docker USER or `"root"`)
fn resolve_remote_user(
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

/// Build lifecycle entries for a phase from the metadata label, falling back to
/// the config value if no metadata is available.
fn lifecycle_entries_for_phase(
    metadata: Option<&str>,
    config: &serde_json::Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    metadata.map_or_else(
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
        |meta_json| cella_features::lifecycle_from_metadata_label(meta_json, phase),
    )
}

/// Convert `cella_env::FileUpload` items to `cella_docker::File`.
fn convert_uploads(uploads: &[cella_env::FileUpload]) -> Vec<FileToUpload> {
    uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect()
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

/// Query the daemon for control port + auth token, returning env vars to inject.
async fn query_daemon_env(container_nm: &str) -> Vec<String> {
    if let Some(mgmt_sock) = cella_env::git_credential::daemon_management_socket_path()
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
    upload_ssh_files(client, container_id, &post_start.file_uploads, remote_user).await;
    install_credential_helper(client, container_id, post_start.credential_helper.as_ref()).await;
    apply_git_config(
        client,
        container_id,
        &post_start.git_config_commands,
        remote_user,
    )
    .await;
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

/// Upload the credential helper script into the container.
async fn install_credential_helper(
    client: &DockerClient,
    container_id: &str,
    helper: Option<&cella_env::FileUpload>,
) {
    let Some(helper) = helper else {
        return;
    };
    let helper_file = FileToUpload {
        path: helper.container_path.clone(),
        content: helper.content.clone(),
        mode: helper.mode,
    };
    if let Err(e) = client.upload_files(container_id, &[helper_file]).await {
        warn!("Failed to install credential helper: {e}");
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

// ── Shared container-operation helpers ──────────────────────────────────────

/// Create a directory inside the container with the given mode (as root).
async fn mkdir_in_container(
    client: &DockerClient,
    container_id: &str,
    dir: &str,
    mode: u32,
) -> Result<cella_docker::ExecResult, cella_docker::CellaDockerError> {
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
async fn chown_in_container(
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

/// Create a directory, upload files, and fix ownership -- the shared pattern
/// used by both `seed_gh_credentials` and `seed_claude_config`.
///
/// Returns `true` on success, `false` on any step failure.
async fn upload_to_container(
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
        // For Claude config we still chown even on upload failure, so always chown.
    }

    chown_in_container(client, container_id, remote_user, dir).await;
    true
}

/// Run a sequence of origin-tracked lifecycle entries.
async fn run_lifecycle_entries(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    entries: &[cella_features::LifecycleEntry],
) -> Result<(), CellaDockerError> {
    for entry in entries {
        run_lifecycle_phase(ctx, phase, &entry.command, &entry.origin).await?;
    }
    Ok(())
}

/// Run all lifecycle phases for a first-create scenario.
async fn run_all_lifecycle_phases(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
) -> Result<(), Box<dyn std::error::Error>> {
    let phases = [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    for phase in phases {
        let empty = Vec::new();
        let entries = resolved_features.map_or(&empty, |rf| match phase {
            "onCreateCommand" => &rf.container_config.lifecycle.on_create,
            "postCreateCommand" => &rf.container_config.lifecycle.post_create,
            "postStartCommand" => &rf.container_config.lifecycle.post_start,
            "postAttachCommand" => &rf.container_config.lifecycle.post_attach,
            _ => &empty,
        });

        run_lifecycle_entries(lc_ctx, phase, entries).await?;

        if entries.is_empty()
            && let Some(cmd) = config.get(phase)
            && !cmd.is_null()
        {
            run_lifecycle_phase(lc_ctx, phase, cmd, "devcontainer.json").await?;
        }
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

    if config_exists_in_container(
        client,
        container_id,
        remote_user,
        &cella_env::gh_credential::gh_config_exists_in_container(&config_dir),
    )
    .await
    {
        tracing::debug!("gh credentials already present in container, skipping seed");
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
        info!("Seeded gh CLI credentials into container");
    }
}

/// Check if a config already exists in the container (runs a test command).
async fn config_exists_in_container(
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
    settings: &cella_config::ClaudeCode,
) {
    let claude_dir = cella_env::claude_code::claude_dir_for_user(remote_user);

    if config_exists_in_container(
        client,
        container_id,
        remote_user,
        &cella_env::claude_code::claude_config_exists_command(&claude_dir),
    )
    .await
    {
        tracing::debug!("Claude config already present in container, skipping seed");
        return;
    }

    let Some(uploads) =
        cella_env::claude_code::prepare_claude_config(remote_user, workspace_root, settings)
    else {
        return;
    };

    upload_to_container(
        client,
        container_id,
        remote_user,
        &claude_dir,
        &uploads,
        "Claude config",
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

    let docker_files = convert_uploads(&uploads);

    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        tracing::debug!("Failed to re-sync Claude auth: {e}");
    }
}

/// Check if Claude Code is already installed at the desired version.
/// Returns `true` if already installed and no (re)install is needed.
async fn is_claude_code_installed(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    version: &str,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) -> bool {
    let version_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, "claude --version"),
                user: Some(remote_user.to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await;

    if let Ok(result) = &version_check
        && result.exit_code == 0
    {
        let installed = result.stdout.trim();
        if version == "latest" || version == "stable" {
            tracing::debug!("Claude Code already installed: {installed}");
            return true;
        }
        if installed.contains(version) {
            tracing::debug!("Claude Code already at version {version}: {installed}");
            return true;
        }
    }
    false
}

/// Check if the container is Alpine-based.
async fn is_alpine_container(client: &DockerClient, container_id: &str) -> bool {
    client
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
        .is_ok_and(|r| r.exit_code == 0)
}

/// Detect Alpine and install Claude Code native dependencies if needed.
/// Returns `true` if the container is Alpine-based.
async fn ensure_alpine_claude_deps(client: &DockerClient, container_id: &str) -> bool {
    let is_alpine = is_alpine_container(client, container_id).await;

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
    is_alpine
}

async fn install_claude_code(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::ClaudeCode,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) {
    if is_claude_code_installed(
        client,
        container_id,
        remote_user,
        &settings.version,
        probed_env,
    )
    .await
    {
        return;
    }

    let is_alpine = ensure_alpine_claude_deps(client, container_id).await;
    run_claude_install(
        client,
        container_id,
        remote_user,
        &settings.version,
        is_alpine,
        probed_env,
    )
    .await;
}

/// Execute the Claude Code install script inside the container.
async fn run_claude_install(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    version: &str,
    is_alpine: bool,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) {
    if version != "latest" && version != "stable" {
        info!("Installing Claude Code v{version} (native installer will attempt version pinning)");
    }

    let install_cmd = format!("curl -fsSL https://claude.ai/install.sh | bash -s {version}");
    info!("Installing Claude Code ({version})...");

    let mut env = tool_exec_env(probed_env).unwrap_or_default();
    if is_alpine {
        env.push("USE_BUILTIN_RIPGREP=0".to_string());
    }
    let env = if env.is_empty() { None } else { Some(env) };

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_cmd],
                user: Some(remote_user.to_string()),
                env,
                working_dir: None,
            },
        )
        .await;

    log_install_result(result);
}

/// Log the result of a Claude Code installation attempt.
fn log_install_result(result: Result<cella_docker::ExecResult, CellaDockerError>) {
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

// ---------------------------------------------------------------------------
// Codex / Gemini CLI installation (npm-based)
// ---------------------------------------------------------------------------

/// Extract PATH from the probed user environment for tool exec calls.
///
/// Returns `Some(vec!["PATH=..."])` when the probed env contains PATH,
/// `None` otherwise (caller should fall back to a login shell).
fn tool_exec_env(
    probed_env: Option<&std::collections::HashMap<String, String>>,
) -> Option<Vec<String>> {
    probed_env
        .and_then(|env| env.get("PATH"))
        .map(|path| vec![format!("PATH={path}")])
}

/// Build the shell command prefix for a tool exec call.
///
/// When the probed env is available (and thus PATH will be passed via `env`),
/// uses a plain `sh -c`. Otherwise falls back to a login shell (`sh -l -c`)
/// so that `/etc/profile.d/` scripts (e.g. nvm) are sourced.
fn tool_shell_cmd(
    probed_env: Option<&std::collections::HashMap<String, String>>,
    inner_cmd: &str,
) -> Vec<String> {
    if probed_env.and_then(|e| e.get("PATH")).is_some() {
        vec!["sh".to_string(), "-c".to_string(), inner_cmd.to_string()]
    } else {
        vec![
            "sh".to_string(),
            "-l".to_string(),
            "-c".to_string(),
            inner_cmd.to_string(),
        ]
    }
}

/// Ensure Node.js and npm are available in the container.
///
/// Uses the probed user environment PATH (from `userEnvProbe`) to detect
/// npm installed by devcontainer features (e.g. nvm). Falls back to a login
/// shell when no probed env is available. If npm is still not found, attempts
/// to install Node.js via the system package manager (apt-get or apk).
/// Returns `true` if npm is available after the check.
async fn ensure_node_available(
    client: &DockerClient,
    container_id: &str,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) -> bool {
    let npm_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, "command -v npm"),
                user: Some("root".to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await;

    if npm_check.is_ok_and(|r| r.exit_code == 0) {
        return true;
    }

    info!("npm not found, installing Node.js...");
    let install_cmd = if is_alpine_container(client, container_id).await {
        "apk add --no-cache nodejs npm"
    } else {
        "apt-get update -qq && apt-get install -y -qq nodejs npm"
    };

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_cmd.to_string()],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    match &result {
        Ok(r) if r.exit_code == 0 => {
            info!("Node.js installed successfully");
            true
        }
        Ok(r) => {
            warn!(
                "Node.js installation failed (exit {}): {}",
                r.exit_code,
                r.stderr.trim()
            );
            false
        }
        Err(e) => {
            warn!("Node.js installation failed: {e}");
            false
        }
    }
}

/// Check if an npm-installed CLI tool is already present at the desired version.
async fn is_npm_tool_installed(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    binary_name: &str,
    version: &str,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) -> bool {
    let version_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, &format!("{binary_name} --version")),
                user: Some(remote_user.to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await;

    if let Ok(result) = &version_check
        && result.exit_code == 0
    {
        let installed = result.stdout.trim();
        if version == "latest" {
            tracing::debug!("{binary_name} already installed: {installed}");
            return true;
        }
        if installed.contains(version) {
            tracing::debug!("{binary_name} already at version {version}: {installed}");
            return true;
        }
    }
    false
}

/// Install an npm package globally inside the container.
async fn npm_install_global(
    client: &DockerClient,
    container_id: &str,
    package: &str,
    version: &str,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) -> Result<cella_docker::ExecResult, CellaDockerError> {
    let pkg = if version == "latest" {
        package.to_string()
    } else {
        format!("{package}@{version}")
    };

    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, &format!("npm install -g {pkg}")),
                user: Some("root".to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await
}

/// Log the result of an npm tool installation attempt.
fn log_npm_install_result(
    tool_name: &str,
    result: Result<cella_docker::ExecResult, CellaDockerError>,
) {
    match result {
        Ok(r) if r.exit_code == 0 => {
            info!("{tool_name} installed successfully");
        }
        Ok(r) => {
            warn!(
                "{tool_name} installation exited with code {}: {}",
                r.exit_code,
                r.stderr.trim()
            );
        }
        Err(e) => {
            warn!("{tool_name} installation failed: {e}");
        }
    }
}

/// Install `OpenAI` Codex CLI inside the container via npm.
///
/// Checks if already installed, ensures Node.js/npm are available,
/// then runs `npm install -g @openai/codex`.
async fn install_codex(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::Codex,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) {
    if is_npm_tool_installed(
        client,
        container_id,
        remote_user,
        "codex",
        &settings.version,
        probed_env,
    )
    .await
    {
        return;
    }

    if !ensure_node_available(client, container_id, probed_env).await {
        warn!("Cannot install Codex: Node.js/npm not available");
        return;
    }

    info!("Installing Codex ({})...", settings.version);
    let result = npm_install_global(
        client,
        container_id,
        "@openai/codex",
        &settings.version,
        probed_env,
    )
    .await;
    log_npm_install_result("Codex", result);
}

/// Install Google Gemini CLI inside the container via npm.
///
/// Checks if already installed, ensures Node.js/npm are available,
/// then runs `npm install -g @google/gemini-cli`.
async fn install_gemini(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::Gemini,
    probed_env: Option<&std::collections::HashMap<String, String>>,
) {
    if is_npm_tool_installed(
        client,
        container_id,
        remote_user,
        "gemini",
        &settings.version,
        probed_env,
    )
    .await
    {
        return;
    }

    if !ensure_node_available(client, container_id, probed_env).await {
        warn!("Cannot install Gemini CLI: Node.js/npm not available");
        return;
    }

    info!("Installing Gemini CLI ({})...", settings.version);
    let result = npm_install_global(
        client,
        container_id,
        "@google/gemini-cli",
        &settings.version,
        probed_env,
    )
    .await;
    log_npm_install_result("Gemini CLI", result);
}
