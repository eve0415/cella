use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{debug, info, warn};

use cella_backend::{
    BackendError, ContainerBackend, ContainerInfo, ContainerState, ExecOptions, ImageDetails,
    LifecycleContext, MountConfig, container_labels, container_name,
};
use cella_config::resolve::{self, ResolvedConfig};

mod lifecycle;

pub use lifecycle::{WaitForPhase, run_all_lifecycle_phases, run_lifecycle_phases_with_wait_for};
use lifecycle::{check_and_run_content_update, run_lifecycle_entries, write_content_hash};

use super::image::ensure_image;

/// Start a dev container for the current workspace.
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // CLI arg structs naturally accumulate boolean flags
pub struct UpArgs {
    #[command(flatten)]
    pub verbose: super::VerboseArgs,

    /// Rebuild the container image before starting.
    #[arg(long)]
    pub(crate) rebuild: bool,

    /// Do not use cache when building the image.
    #[arg(long)]
    pub(crate) build_no_cache: bool,

    /// Remove existing container before starting.
    #[arg(long)]
    pub(crate) remove_existing_container: bool,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    pub(crate) workspace_folder: Option<PathBuf>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    pub(crate) docker_host: Option<String>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    pub(crate) file: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub(crate) output: OutputFormat,

    /// Strictness level for validation ("host-requirements" to fail on unmet requirements).
    #[arg(long)]
    pub(crate) strict: Vec<String>,

    /// Skip SHA256 checksum verification for agent binary download.
    #[arg(long)]
    pub(crate) skip_checksum: bool,

    /// Target a worktree branch's container by branch name.
    #[arg(long)]
    pub(crate) branch: Option<String>,

    /// Start container without network blocking rules (proxy forwarding still active).
    #[arg(long)]
    pub(crate) no_network_rules: bool,
}

/// Output format for container commands.
#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

impl UpArgs {
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Text)
    }
}

pub use cella_orchestrator::up::NetworkRulePolicy;

/// Holds resolved state for an `up` invocation, shared across all code paths.
pub struct UpContext {
    pub(crate) resolved: ResolvedConfig,
    pub client: Box<dyn ContainerBackend>,
    pub container_nm: String,
    pub(crate) remote_env: Vec<String>,
    workspace_folder_from_config: Option<String>,
    default_workspace_folder: String,
    pub(crate) progress: crate::progress::Progress,
    pub(crate) output: OutputFormat,
    pub(crate) remove_container: bool,
    pub(crate) build_no_cache: bool,
    pub(crate) skip_checksum: bool,
    /// Extra Docker labels to merge into the container (e.g., worktree labels).
    extra_labels: std::collections::HashMap<String, String>,
    /// Network rule enforcement policy.
    network_rules: NetworkRulePolicy,
}

/// Resolved image configuration for container creation.
struct ImageConfig {
    image_env: Vec<String>,
    remote_user: String,
    env_fwd: cella_env::EnvForwarding,
    create_opts: cella_orchestrator::config_map::CreateContainerOptions,
}

impl UpContext {
    pub(crate) async fn new(
        args: &UpArgs,
        progress: crate::progress::Progress,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = crate::commands::resolve_workspace_folder(args.workspace_folder.as_deref())?;

        let remove_container = args.rebuild || args.remove_existing_container;

        // 1. Resolve config
        let resolved = progress
            .run_step("Resolving devcontainer configuration...", async {
                resolve::config(&cwd, args.file.as_deref())
            })
            .await?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        // 2. Connect to backend
        let client = super::resolve_backend_for_command(backend, args.docker_host.as_deref())?;
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
            progress,
            output: args.output.clone(),
            remove_container,
            build_no_cache: args.build_no_cache,
            skip_checksum: args.skip_checksum,
            extra_labels: std::collections::HashMap::new(),
            network_rules: if args.no_network_rules {
                NetworkRulePolicy::Skip
            } else {
                NetworkRulePolicy::Enforce
            },
        })
    }

    /// Create an `UpContext` for a workspace path (used by `cella branch`).
    ///
    /// Unlike `new()`, this does not take `UpArgs` — it accepts the workspace
    /// path and options directly. Always sets `remove_container` and
    /// `build_no_cache` to false.
    pub async fn for_workspace(
        workspace_path: &std::path::Path,
        docker_host: Option<&str>,
        extra_labels: std::collections::HashMap<String, String>,
        progress: crate::progress::Progress,
        output: OutputFormat,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = workspace_path
            .canonicalize()
            .unwrap_or_else(|_| workspace_path.to_path_buf());

        let resolved = progress
            .run_step("Resolving devcontainer configuration...", async {
                resolve::config(&cwd, None)
            })
            .await?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        let client = super::resolve_backend_for_command(backend, docker_host)?;
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
            progress,
            output,
            remove_container: false,
            build_no_cache: false,
            skip_checksum: false,
            extra_labels,
            network_rules: NetworkRulePolicy::Enforce,
        })
    }

    pub(crate) const fn config(&self) -> &serde_json::Value {
        &self.resolved.config
    }

    pub(crate) fn config_name(&self) -> Option<&str> {
        self.config().get("name").and_then(|v| v.as_str())
    }

    pub(crate) fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder_from_config.as_deref()
    }

    pub(crate) fn workspace_folder_str(&self) -> &str {
        self.workspace_folder_from_config
            .as_deref()
            .unwrap_or(&self.default_workspace_folder)
    }

    /// Build a `LifecycleContext` for running lifecycle phases in this container.
    fn build_lifecycle_ctx<'a>(
        &'a self,
        container_id: &'a str,
        user: &'a str,
        env: &'a [String],
    ) -> LifecycleContext<'a> {
        let progress_ref = self.progress.clone();
        LifecycleContext {
            client: &*self.client,
            container_id,
            user: Some(user),
            env,
            working_dir: self.workspace_folder(),
            is_text: self.progress.is_enabled(),
            on_output: if self.progress.is_enabled() {
                Some(Box::new(move |line| progress_ref.println(line)))
            } else {
                None
            },
        }
    }

    pub(crate) fn probe_type(&self) -> &str {
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

    /// Detect the container architecture from the backend runtime.
    async fn detect_arch(&self) -> String {
        self.client
            .detect_container_arch()
            .await
            .unwrap_or_else(|e| {
                warn!("Container arch detection failed, defaulting to x86_64: {e}");
                "x86_64".to_string()
            })
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
        let env_fwd = cella_env::prepare_env_forwarding(config, remote_user, None);
        self.progress
            .run_step(
                "Configuring environment...",
                inject_post_start(
                    self.client.as_ref(),
                    container_id,
                    &env_fwd.post_start,
                    remote_user,
                ),
            )
            .await;

        // Detect user's shell for probing (use their actual shell, not /bin/sh)
        let shell =
            super::shell_detect::detect_shell(self.client.as_ref(), container_id, remote_user)
                .await;

        // Probe user environment first so tool installs can use feature-provided PATH
        // (e.g., nvm adds /usr/local/share/nvm/current/bin via login shell profiles)
        let probed_env = self
            .progress
            .run_step(
                "Running userEnvProbe...",
                super::env_cache::probe_and_cache_user_env(
                    self.client.as_ref(),
                    container_id,
                    remote_user,
                    self.probe_type(),
                    &shell,
                ),
            )
            .await;

        // Sequential prerequisites + tool installation
        let settings = cella_config::Settings::load(&self.resolved.workspace_root);
        if settings.tools.claude_code.forward_config {
            create_claude_home_symlink(self.client.as_ref(), container_id, remote_user).await;
            setup_plugin_manifests(self.client.as_ref(), container_id, remote_user).await;
        }

        let any_tool = settings.tools.claude_code.enabled
            || settings.tools.codex.enabled
            || settings.tools.gemini.enabled;
        self.install_tools(container_id, remote_user, &settings, probed_env.as_ref())
            .await;

        // Re-probe after tool installation to capture PATH changes
        // (e.g., Claude Code installer adds ~/.local/bin to shell profiles)
        let final_probed = if any_tool {
            self.progress
                .run_step(
                    "Updating environment cache...",
                    super::env_cache::probe_and_cache_user_env(
                        self.client.as_ref(),
                        container_id,
                        remote_user,
                        self.probe_type(),
                        &shell,
                    ),
                )
                .await
                .or(probed_env)
        } else {
            probed_env
        };

        let lifecycle_env = final_probed.as_ref().map_or_else(
            || self.remote_env.clone(),
            |probed| cella_env::user_env_probe::merge_env(probed, &self.remote_env),
        );

        (final_probed, lifecycle_env)
    }

    /// Handle an already-running container (no rebuild requested, no `--build-no-cache`).
    async fn handle_running(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<UpResult, Box<dyn std::error::Error>> {
        super::ensure_cella_daemon().await;

        // Check for previous background lifecycle failure
        if let Ok(result) = self
            .client
            .exec_command(
                &container.id,
                &ExecOptions {
                    cmd: vec![
                        "cat".to_string(),
                        "/tmp/.cella/lifecycle_status.json".to_string(),
                    ],
                    user: Some(remote_user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await
            && result.exit_code == 0
            && result.stdout.contains("\"failed\"")
        {
            self.progress
                .warn("Previous background lifecycle phase failed.");
            self.progress
                .hint("Run `cella logs --lifecycle` for details.");
        }

        // Always update .daemon_addr (daemon may have restarted)
        write_daemon_addr_to_volume(self.client.as_ref()).await;

        // Version-aware agent restart: if the container was created with a
        // different cella version, repopulate the volume and restart the agent.
        let container_version = container
            .labels
            .get("dev.cella.version")
            .map_or("unknown", String::as_str);
        let current_version = env!("CARGO_PKG_VERSION");
        if container_version != current_version {
            info!(
                "Version change detected ({container_version} -> {current_version}), updating agent"
            );
            let agent_arch = self.detect_arch().await;
            if let Err(e) = self
                .client
                .ensure_agent_provisioned(current_version, &agent_arch, self.skip_checksum)
                .await
            {
                warn!("Failed to repopulate agent volume: {e}");
            }
            self.register_with_daemon(&container.id).await;
            restart_agent_in_container(self.client.as_ref(), &container.id).await;
        }

        let (_probed_env, lifecycle_env) =
            self.prepare_container_env(&container.id, remote_user).await;

        let metadata = container.labels.get("devcontainer.metadata");

        // Check for workspace content changes (updateContentCommand)
        let lc_ctx_content = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        check_and_run_content_update(
            &lc_ctx_content,
            self.config(),
            metadata.map(String::as_str),
            &self.resolved.workspace_root,
            &self.progress,
        )
        .await?;

        // Already running -- run postAttachCommand from metadata (includes features)
        let entries = lifecycle_entries_for_phase(
            metadata.map(String::as_str),
            self.config(),
            "postAttachCommand",
        );
        let lc_ctx = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        run_lifecycle_entries(&lc_ctx, "postAttachCommand", &entries, &self.progress).await?;

        Ok(UpResult {
            container_id: container.id.clone(),
            remote_user: remote_user.to_string(),
            outcome: "running".to_string(),
            workspace_folder: self.workspace_folder_str().to_string(),
        })
    }

    /// Handle a stopped container: try to restart it.
    ///
    /// Returns `Ok(Some(result))` if the container was successfully restarted,
    /// `Ok(None)` if restart failed and the container was removed (caller should
    /// fall through to the create path).
    async fn handle_stopped(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<Option<UpResult>, Box<dyn std::error::Error>> {
        super::ensure_cella_daemon().await;

        // Warn if config has changed
        if let Some(old_hash) = &container.config_hash
            && *old_hash != self.resolved.config_hash
        {
            self.progress
                .warn("Config has changed since this container was created.");
            self.progress
                .hint("Run `cella up --rebuild` to recreate with the updated config.");
        }

        // Warn if Docker runtime has changed
        let current_runtime = cella_env::platform::detect_runtime();
        if let Some(old_runtime) = container.labels.get("dev.cella.docker_runtime") {
            let current_label = current_runtime.as_label();
            if old_runtime != current_label {
                self.progress.warn(&format!(
                    "Docker runtime changed ({old_runtime} \u{2192} {current_label})."
                ));
                self.progress
                    .hint("Run `cella up --rebuild` to recreate with the updated runtime.");
            }
        }

        // Repopulate agent volume before starting (may have been updated since
        // the container was created, and the old versioned binary path in CMD
        // would fail if the volume was repopulated by another `cella up`).
        let agent_arch = self.detect_arch().await;
        let version = env!("CARGO_PKG_VERSION");
        self.progress
            .run_step(
                "Populating agent volume...",
                self.client
                    .ensure_agent_provisioned(version, &agent_arch, self.skip_checksum),
            )
            .await?;

        // Write .daemon_addr so the agent can discover the current daemon
        write_daemon_addr_to_volume(self.client.as_ref()).await;

        // Attempt to start directly -- let Docker validate mounts
        let step = self.progress.step("Starting container...");
        let start_result = self.client.start_container(&container.id).await;
        if start_result.is_ok() {
            step.finish();
        } else {
            step.fail("failed to start");
        }

        match start_result {
            Ok(()) => {
                verify_container_running(self.client.as_ref(), &container.id).await?;

                // Register with daemon (missing from the original code —
                // without this the daemon rejects the agent with "unknown container")
                self.register_with_daemon(&container.id).await;

                // Kill + restart agent so it picks up the new binary and
                // daemon address from the updated .daemon_addr file
                restart_agent_in_container(self.client.as_ref(), &container.id).await;

                let (_probed_env, lifecycle_env) =
                    self.prepare_container_env(&container.id, remote_user).await;

                // Run lifecycle from metadata label (includes features)
                let metadata = container.labels.get("devcontainer.metadata");
                let lc_ctx = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
                for phase in ["postStartCommand", "postAttachCommand"] {
                    let entries = lifecycle_entries_for_phase(
                        metadata.map(String::as_str),
                        self.config(),
                        phase,
                    );
                    run_lifecycle_entries(&lc_ctx, phase, &entries, &self.progress).await?;
                }

                // Prune old agent versions from the volume (non-fatal)
                let prune_version = env!("CARGO_PKG_VERSION");
                if let Err(e) = self.client.prune_old_agent_versions(prune_version).await {
                    debug!("Agent version pruning failed: {e}");
                }

                Ok(Some(UpResult {
                    container_id: container.id.clone(),
                    remote_user: remote_user.to_string(),
                    outcome: "started".to_string(),
                    workspace_folder: self.workspace_folder_str().to_string(),
                }))
            }
            Err(e) => {
                warn!("Failed to start existing container: {e}");
                self.progress
                    .warn(&format!("Could not start existing container: {e}"));
                self.progress.hint("Recreating container...");
                let _ = self.client.remove_container(&container.id, false).await;
                // Fall through to creation path
                Ok(None)
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
            if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
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
            "dev.cella.version".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
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

        let ports_attrs = cella_orchestrator::config_map::ports::parse_ports_attributes(config);
        let other_ports_attrs =
            cella_orchestrator::config_map::ports::parse_other_ports_attributes(config);
        labels.insert(
            "dev.cella.ports_attributes".to_string(),
            cella_orchestrator::config_map::ports::serialize_ports_attributes_label(
                &ports_attrs,
                other_ports_attrs.as_ref(),
            ),
        );

        if let Some(action) = config.get("shutdownAction").and_then(|v| v.as_str()) {
            labels.insert("dev.cella.shutdown_action".to_string(), action.to_string());
        }

        // Merge extra labels (e.g., worktree labels from `cella branch`)
        labels.extend(self.extra_labels.clone());

        labels
    }

    /// Merge forwarding mounts, env vars, daemon env, and agent volume into create options.
    ///
    /// Tool config mounts (Claude Code, Codex, Gemini) are added via [`add_tool_config_mounts`].
    async fn apply_env_and_mounts(
        &self,
        create_opts: &mut cella_backend::CreateContainerOptions,
        env_fwd: &cella_env::EnvForwarding,
        image_env: &[String],
        remote_user: &str,
        settings: &cella_config::Settings,
        agent_arch: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Forwarding mounts
        for m in &env_fwd.mounts {
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: m.source.clone(),
                target: m.target.clone(),
                consistency: None,
            });
        }

        add_tool_config_mounts(create_opts, settings, remote_user);

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
        let daemon_env = query_daemon_env(&self.container_nm, self.client.host_gateway()).await;
        if !daemon_env.is_empty() {
            if create_opts.env.is_empty() {
                create_opts.env = image_env.to_vec();
            }
            create_opts.env.extend(daemon_env);
        }

        // Agent volume mount and env vars
        let version = env!("CARGO_PKG_VERSION");
        let agent_step = self.progress.step("Populating agent volume...");
        match self
            .client
            .ensure_agent_provisioned(version, agent_arch, self.skip_checksum)
            .await
        {
            Ok(()) => agent_step.finish(),
            Err(e @ BackendError::AgentChecksumMismatch { .. }) => {
                agent_step.fail("checksum mismatch");
                return Err(e.into());
            }
            Err(e) => {
                agent_step.fail("failed");
                warn!("Failed to populate agent volume: {e}");
                self.progress
                    .warn("Port forwarding and BROWSER interception will not work.");
                self.progress
                    .hint(&format!("Agent volume population failed: {e}"));
            }
        }

        // Write .daemon_addr to volume so agents can discover the daemon
        write_daemon_addr_to_volume(self.client.as_ref()).await;

        let agent_env = cella_docker::config_map::env::agent_env_vars();
        if create_opts.env.is_empty() {
            create_opts.env = image_env.to_vec();
        }
        create_opts.env.extend(agent_env);

        // If the workspace is a linked git worktree, mount the parent repo's
        // .git directory at the same host path so gitdir references resolve.
        if let Some(parent_git) = cella_git::parent_git_dir(&self.resolved.workspace_root) {
            let canonical = parent_git
                .canonicalize()
                .unwrap_or_else(|_| parent_git.clone());
            let path_str = canonical.to_string_lossy().to_string();
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: path_str.clone(),
                target: path_str,
                consistency: None,
            });
        }

        let (vol_name, vol_target, _ro) = self.client.agent_volume_mount();
        if !vol_name.is_empty() {
            create_opts.mounts.push(MountConfig {
                mount_type: "volume".to_string(),
                source: vol_name,
                target: vol_target,
                consistency: None,
            });
        }

        Ok(())
    }

    /// Start the container, connect networks, and register with the daemon.
    async fn start_and_register(
        &self,
        container_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let start_label = if self.progress.is_verbose() {
            let short_id = &container_id[..12.min(container_id.len())];
            format!("Starting container: {short_id}...")
        } else {
            "Starting container...".to_string()
        };
        self.progress
            .run_step(&start_label, self.client.start_container(container_id))
            .await?;
        verify_container_running(self.client.as_ref(), container_id).await?;

        if let Err(e) = self
            .client
            .ensure_container_network(container_id, &self.resolved.workspace_root)
            .await
        {
            warn!("Failed to connect container to networks: {e}");
        }

        self.register_with_daemon(container_id).await;
        Ok(())
    }

    /// Register the container with the daemon for port management.
    pub(crate) async fn register_with_daemon(&self, container_id: &str) {
        let config = self.config();
        let container_ip = self
            .client
            .get_container_ip(container_id)
            .await
            .unwrap_or(None);

        let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
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

        let ports_attrs = cella_orchestrator::config_map::ports::parse_ports_attributes(config);
        let other_ports_attrs =
            cella_orchestrator::config_map::ports::parse_other_ports_attributes(config);
        let shutdown_action = config
            .get("shutdownAction")
            .and_then(|v| v.as_str())
            .map(String::from);
        let req = cella_port::protocol::ManagementRequest::RegisterContainer {
            container_id: container_id.to_string(),
            container_name: self.container_nm.clone(),
            container_ip,
            ports_attributes: ports_attrs,
            other_ports_attributes: other_ports_attrs,
            forward_ports,
            shutdown_action,
        };
        match cella_daemon::management::send_management_request(&mgmt_sock, &req).await {
            Ok(resp) => {
                debug!("Container registered with daemon: {resp:?}");
            }
            Err(e) => {
                warn!("Failed to register container with daemon: {e}");
            }
        }
    }

    /// Run post-create setup: UID update, env injection, credentials, Claude Code, userEnvProbe.
    pub(crate) async fn post_create_setup(
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
            && let Err(e) = self
                .client
                .update_remote_user_uid(container_id, remote_user, &self.resolved.workspace_root)
                .await
        {
            warn!("Failed to update remote user UID: {e}");
        }

        // Inject post-start environment forwarding
        self.progress
            .run_step(
                "Configuring environment...",
                inject_post_start(
                    self.client.as_ref(),
                    container_id,
                    &env_fwd.post_start,
                    remote_user,
                ),
            )
            .await;

        // Add /cella/bin to PATH in shell profiles so `cella` CLI is discoverable.
        inject_cella_path(self.client.as_ref(), container_id, remote_user).await;

        // Seed gh CLI credentials (first create only)
        if settings.credentials.gh {
            seed_gh_credentials(
                self.client.as_ref(),
                container_id,
                &self.resolved.workspace_root,
                remote_user,
            )
            .await;
        }

        // Detect user's shell for probing (use their actual shell, not /bin/sh)
        let shell =
            super::shell_detect::detect_shell(self.client.as_ref(), container_id, remote_user)
                .await;

        // Probe user environment first so tool installs can use feature-provided PATH
        // (e.g., nvm adds /usr/local/share/nvm/current/bin via login shell profiles)
        let probed_env = self
            .progress
            .run_step(
                "Running userEnvProbe...",
                super::env_cache::probe_and_cache_user_env(
                    self.client.as_ref(),
                    container_id,
                    remote_user,
                    self.probe_type(),
                    &shell,
                ),
            )
            .await;

        // Fix /tmp permissions (must be world-writable with sticky bit).
        // upload_files can reset /tmp to 755 via tar directory entries;
        // some base images may also lack the sticky bit.
        let _ = self
            .client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "sh".into(),
                        "-c".into(),
                        "chmod 1777 /tmp 2>/dev/null || true".into(),
                    ],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;

        // Create home path symlink and populate plugin manifests
        if settings.tools.claude_code.forward_config {
            create_claude_home_symlink(self.client.as_ref(), container_id, remote_user).await;
            setup_plugin_manifests(self.client.as_ref(), container_id, remote_user).await;
        }

        // Install AI coding tools
        let any_tool = settings.tools.claude_code.enabled
            || settings.tools.codex.enabled
            || settings.tools.gemini.enabled;
        self.install_tools(container_id, remote_user, settings, probed_env.as_ref())
            .await;

        // Re-probe after tool installation to capture PATH changes
        // (e.g., Claude Code installer adds ~/.local/bin to shell profiles)
        let final_probed = if any_tool {
            self.progress
                .run_step(
                    "Updating environment cache...",
                    super::env_cache::probe_and_cache_user_env(
                        self.client.as_ref(),
                        container_id,
                        remote_user,
                        self.probe_type(),
                        &shell,
                    ),
                )
                .await
                .or(probed_env)
        } else {
            probed_env
        };

        let lifecycle_env = final_probed.as_ref().map_or_else(
            || remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, remote_env),
        );

        (final_probed, lifecycle_env)
    }

    /// Forward config and install AI coding tools (Claude Code, Codex, Gemini).
    ///
    /// Delegates to [`cella_orchestrator::tool_install::install_tools`].
    async fn install_tools(
        &self,
        container_id: &str,
        remote_user: &str,
        settings: &cella_config::Settings,
        probed_env: Option<&std::collections::HashMap<String, String>>,
    ) {
        let (sender, renderer) = crate::progress::bridge(&self.progress);
        cella_orchestrator::tool_install::install_tools(
            self.client.as_ref(),
            container_id,
            remote_user,
            settings,
            probed_env,
            &sender,
        )
        .await;
        drop(sender);
        let _ = renderer.await;
    }

    /// Resolve image metadata, environment forwarding, and container creation options.
    async fn resolve_image_config(
        &self,
        config: &serde_json::Value,
        img_name: &str,
        base_image_details: ImageDetails,
        resolved_features: Option<&cella_features::ResolvedFeatures>,
        agent_arch: &str,
    ) -> ImageConfig {
        let image_env = base_image_details.env;
        let image_meta_user = base_image_details
            .metadata
            .as_deref()
            .map(|m| cella_features::parse_image_metadata(m).1);
        let remote_user =
            resolve_remote_user(config, image_meta_user.as_ref(), &base_image_details.user);

        super::ensure_cella_daemon().await;

        // Build network proxy forwarding config from settings + devcontainer.json.
        let settings = cella_config::Settings::load(&self.resolved.workspace_root);
        let toml_net = settings.network.to_network_config();
        let toml_mode_override = settings.network.mode_override();

        // Extract customizations.cella.network from devcontainer.json.
        let dc_net = config
            .get("customizations")
            .and_then(|c| c.get("cella"))
            .and_then(|c| c.get("network"))
            .and_then(|n| serde_json::from_value::<cella_network::NetworkConfig>(n.clone()).ok());

        // Merge: devcontainer.json is base, cella.toml overrides (only when explicit).
        let merged = cella_network::merge_network_configs(
            dc_net.as_ref(),
            Some(&toml_net),
            toml_mode_override,
        );
        let net_config = cella_network::NetworkConfig {
            mode: merged.mode,
            proxy: merged.proxy,
            rules: merged.rules.into_iter().map(|lr| lr.rule).collect(),
        };

        let skip_rules = self.network_rules == NetworkRulePolicy::Skip;
        let has_rules = net_config.has_rules() && !skip_rules;
        if skip_rules && net_config.has_rules() {
            tracing::info!("Network blocking rules disabled via --no-network-rules");
        }
        let proxy_fwd = cella_env::ProxyForwardingConfig {
            proxy: net_config.proxy.clone(),
            has_blocking_rules: has_rules,
            full_config: if has_rules { Some(net_config) } else { None },
            container_distro: cella_env::ca_bundle::ContainerDistro::Unknown,
        };
        let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user, Some(&proxy_fwd));

        let labels = self.build_labels(
            resolved_features,
            base_image_details.metadata.as_deref(),
            &env_fwd,
            &remote_user,
        );

        let feature_config = resolved_features.map(|r| &r.container_config);
        let image_meta_config = if feature_config.is_none() {
            base_image_details
                .metadata
                .as_deref()
                .map(|m| cella_features::parse_image_metadata(m).0)
        } else {
            None
        };
        let effective_feature_config = feature_config.or(image_meta_config.as_ref());

        let create_opts = cella_orchestrator::config_map::map_config(
            cella_orchestrator::config_map::MapConfigParams {
                config,
                container_name: &self.container_nm,
                image_name: img_name,
                labels,
                workspace_root: &self.resolved.workspace_root,
                feature_config: effective_feature_config,
                image_env: &image_env,
                agent_arch,
            },
        );

        ImageConfig {
            image_env,
            remote_user,
            env_fwd,
            create_opts,
        }
    }

    /// The full build/create/start/lifecycle path for a new container.
    ///
    /// Returns the container ID and remote user on success.
    pub async fn create_and_start(
        &self,
        build_no_cache: bool,
    ) -> Result<CreateResult, Box<dyn std::error::Error>> {
        let config = self.config();
        // Run initializeCommand on host (runs every invocation per spec)
        if let Some(init_cmd) = config.get("initializeCommand") {
            run_host_command("initializeCommand", init_cmd)?;
        }

        // Ensure image (with optional features layer)
        let (img_name, resolved_features, base_image_details) = ensure_image(
            self.client.as_ref(),
            config,
            &self.resolved.workspace_root,
            self.config_name(),
            &self.resolved.config_path,
            build_no_cache,
            &self.progress,
        )
        .await?;
        let agent_arch = self.detect_arch().await;
        let ImageConfig {
            image_env,
            remote_user,
            env_fwd,
            mut create_opts,
        } = self
            .resolve_image_config(
                config,
                &img_name,
                base_image_details,
                resolved_features.as_ref(),
                &agent_arch,
            )
            .await;

        let settings = cella_config::Settings::load(&self.resolved.workspace_root);
        self.apply_env_and_mounts(
            &mut create_opts,
            &env_fwd,
            &image_env,
            &remote_user,
            &settings,
            &agent_arch,
        )
        .await?;

        // Create and start container
        let container_id = if self.progress.is_verbose() {
            let step = self
                .progress
                .step(&format!("Creating container: {}...", self.container_nm));
            let result = self.client.create_container(&create_opts).await;
            match &result {
                Ok(_) => step.finish(),
                Err(e) => step.fail(&e.to_string()),
            }
            result?
        } else {
            self.progress
                .run_step(
                    "Creating container...",
                    self.client.create_container(&create_opts),
                )
                .await?
        };

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
        let lc_ctx = self.build_lifecycle_ctx(&container_id, &remote_user, &lifecycle_env);
        run_lifecycle_phases_with_wait_for(
            &lc_ctx,
            config,
            resolved_features.as_ref(),
            &self.progress,
            WaitForPhase::from_config(config),
        )
        .await?;

        write_content_hash(
            self.client.as_ref(),
            &container_id,
            &remote_user,
            &self.resolved.workspace_root,
        )
        .await;

        Ok(CreateResult {
            container_id: container_id.clone(),
            remote_user: remote_user.clone(),
        })
    }
}

/// Result of creating and starting a container.
pub struct CreateResult {
    pub container_id: String,
    pub remote_user: String,
}

/// Result of ensuring a container is up and ready.
pub struct UpResult {
    pub container_id: String,
    pub remote_user: String,
    pub outcome: String,
    pub workspace_folder: String,
}

impl UpContext {
    /// Ensure a container is up and ready, returning the result without printing output.
    ///
    /// This is the core logic shared by `cella up` and `cella code`.
    /// It handles existing containers (running, stopped) and creates new ones as needed.
    pub async fn ensure_up(
        &self,
        build_no_cache: bool,
        strict: &[String],
    ) -> Result<UpResult, Box<dyn std::error::Error>> {
        // Validate hostRequirements
        if self.config().get("hostRequirements").is_some() {
            let result = cella_orchestrator::host_requirements::validate(
                self.config(),
                &self.resolved.workspace_root,
            );
            for check in &result.checks {
                if !check.met {
                    self.progress.warn(&format!(
                        "Host does not meet requirement: {} (need {}, have {})",
                        check.name, check.required, check.actual
                    ));
                }
            }
            if !result.all_met
                && strict
                    .iter()
                    .any(|s| s == "host-requirements" || s == "all")
            {
                return Err("Host does not meet hostRequirements (--strict mode)".into());
            }
        }

        // Docker Compose branch: if dockerComposeFile is present, delegate to compose flow
        if self.config().get("dockerComposeFile").is_some() {
            // Compose flow still uses the old path with its own output_result calls.
            // For now, compose + code is not supported via ensure_up; callers should
            // use compose_up directly.
            return Err(
                "Docker Compose projects are not yet supported with `cella code`. \
                 Use `cella up` first, then `cella code`."
                    .into(),
            );
        }

        let existing = self
            .client
            .find_container(&self.resolved.workspace_root)
            .await?;

        if let Some(container) = existing {
            let remote_user = self.resolve_remote_user_from_container(&container).await;

            match (&container.state, self.remove_container) {
                (ContainerState::Running, false) if !build_no_cache => {
                    return self.handle_running(&container, &remote_user).await;
                }
                (ContainerState::Stopped, false) => {
                    if let Some(result) = self.handle_stopped(&container, &remote_user).await? {
                        return Ok(result);
                    }
                    // Fall through to create_and_start
                }
                (ContainerState::Running, false) => {
                    // build_no_cache=true with running container: stop, remove, rebuild
                    self.remove_existing(&container, "--build-no-cache").await?;
                }
                (ContainerState::Running, true) => {
                    self.remove_existing(&container, "rebuild").await?;
                }
                (_, true) => {
                    // Rebuild: stop if running, then remove
                    if container.state == ContainerState::Running {
                        self.client.stop_container(&container.id).await?;
                    }
                    self.client.remove_container(&container.id, false).await?;
                }
                (_, false) => {
                    // Created but never started, or other state — remove and recreate
                    self.client.remove_container(&container.id, false).await?;
                }
            }
        }

        let create_result = self.create_and_start(build_no_cache).await?;
        Ok(UpResult {
            container_id: create_result.container_id,
            remote_user: create_result.remote_user,
            outcome: "created".to_string(),
            workspace_folder: self.workspace_folder_str().to_string(),
        })
    }
}

impl UpArgs {
    /// Handle `--branch`: start/restart a worktree branch's container.
    async fn execute_branch(
        &self,
        branch_name: &str,
        progress: crate::progress::Progress,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd)?;
        let worktrees = cella_git::list(&repo_info.root)?;
        let wt = worktrees
            .iter()
            .find(|wt| wt.branch.as_deref() == Some(branch_name))
            .ok_or_else(|| {
                format!(
                    "No worktree for branch '{branch_name}'. \
                     Use `cella branch {branch_name}` to create one."
                )
            })?;

        let extra_labels = cella_backend::worktree_labels(branch_name, &repo_info.root);
        let mut ctx = UpContext::for_workspace(
            &wt.path,
            self.docker_host.as_deref(),
            extra_labels,
            progress,
            self.output.clone(),
            backend,
        )
        .await?;
        ctx.remove_container = self.rebuild || self.remove_existing_container;
        ctx.build_no_cache = self.build_no_cache;
        ctx.skip_checksum = self.skip_checksum;
        ctx.network_rules = if self.no_network_rules {
            NetworkRulePolicy::Skip
        } else {
            NetworkRulePolicy::Enforce
        };

        let result = ctx.ensure_up(self.build_no_cache, &self.strict).await?;
        output_result(
            &ctx.output,
            &result.outcome,
            &result.container_id,
            &result.remote_user,
            &result.workspace_folder,
        );
        Ok(())
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(ref branch_name) = self.branch {
            return self.execute_branch(branch_name, progress, backend).await;
        }

        let ctx = UpContext::new(&self, progress, backend).await?;

        // Docker Compose branch: if dockerComposeFile is present, delegate to compose flow
        if ctx.config().get("dockerComposeFile").is_some() {
            return super::compose_up::compose_up(ctx).await;
        }

        let result = ctx.ensure_up(self.build_no_cache, &self.strict).await?;
        output_result(
            &ctx.output,
            &result.outcome,
            &result.container_id,
            &result.remote_user,
            &result.workspace_folder,
        );
        Ok(())
    }
}

/// Resolve the remote user from config and image metadata.
///
/// Priority: `remoteUser` (config) > `containerUser` (config) > `remoteUser` (image metadata)
/// > `containerUser` (image metadata) > `fallback` (typically Docker USER or `"root"`)
pub fn resolve_remote_user(
    config: &serde_json::Value,
    image_meta_user: Option<&cella_features::ImageMetadataUserInfo>,
    fallback: &str,
) -> String {
    cella_orchestrator::container_setup::resolve_remote_user(config, image_meta_user, fallback)
}

/// Build lifecycle entries for a phase — delegates to orchestrator.
fn lifecycle_entries_for_phase(
    metadata: Option<&str>,
    config: &serde_json::Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    cella_orchestrator::lifecycle::lifecycle_entries_for_phase(metadata, config, phase)
}

pub fn run_host_command(
    phase: &str,
    value: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    cella_orchestrator::container_setup::run_host_command(phase, value)
}

pub fn map_env_object(value: Option<&serde_json::Value>) -> Vec<String> {
    cella_orchestrator::container_setup::map_env_object(value)
}

pub async fn verify_container_running(
    client: &dyn ContainerBackend,
    container_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    cella_orchestrator::container_setup::verify_container_running(client, container_id).await
}

pub fn output_result(
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
            println!("{}", serde_json::to_string(&output).unwrap_or_default());
        }
    }
}

/// Query the daemon for control port + auth token, returning env vars to inject.
///
/// `host_gateway` is the hostname the container uses to reach the host
/// (e.g. `"host.docker.internal"` for Docker, `"host.local"` for Apple Container).
pub async fn query_daemon_env(container_nm: &str, host_gateway: &str) -> Vec<String> {
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
                format!("CELLA_DAEMON_ADDR={host_gateway}:{control_port}"),
                format!("CELLA_DAEMON_TOKEN={control_token}"),
                format!("CELLA_CONTAINER_NAME={container_nm}"),
            ];
        }
    }
    vec![]
}

/// Inject post-start environment forwarding into a running container.
///
/// Uploads SSH config files and sets git config.
/// Never fails — individual steps log warnings and are skipped on error.
pub async fn inject_post_start(
    client: &dyn ContainerBackend,
    container_id: &str,
    post_start: &cella_env::PostStartInjection,
    remote_user: &str,
) {
    cella_orchestrator::container_setup::inject_post_start(
        client,
        container_id,
        post_start,
        remote_user,
    )
    .await;
}

/// Add `/cella/bin` to PATH in the container's shell profile.
async fn inject_cella_path(client: &dyn ContainerBackend, container_id: &str, remote_user: &str) {
    cella_orchestrator::container_setup::inject_cella_path(client, container_id, remote_user).await;
}

// ── Tool config mounts ─────────────────────────────────────────────────────

/// Add bind mounts for tool config directories (Claude Code, Codex, Gemini, nvim, tmux).
fn add_tool_config_mounts(
    create_opts: &mut cella_backend::CreateContainerOptions,
    settings: &cella_config::Settings,
    remote_user: &str,
) {
    cella_orchestrator::tool_install::add_tool_config_mounts(create_opts, settings, remote_user);
}

// ── Shared container-operation helpers (delegated to orchestrator) ─────────

/// Seed gh CLI credentials into a container.
async fn seed_gh_credentials(
    client: &dyn ContainerBackend,
    container_id: &str,
    workspace_root: &std::path::Path,
    remote_user: &str,
) {
    cella_orchestrator::container_setup::seed_gh_credentials(
        client,
        container_id,
        workspace_root,
        remote_user,
    )
    .await;
}

/// Create a symlink from the host's `.claude` path to the container's so that
/// hardcoded paths in plugin manifests resolve transparently.
async fn create_claude_home_symlink(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
    cella_orchestrator::tool_install::create_claude_home_symlink(client, container_id, remote_user)
        .await;
}

/// Populate the tmpfs-backed `~/.claude/plugins/` directory.
async fn setup_plugin_manifests(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
    cella_orchestrator::tool_install::setup_plugin_manifests(client, container_id, remote_user)
        .await;
}

// ── Version skew helpers ─────────────────────────────────────────────────

/// Write the `.daemon_addr` file to the shared agent volume.
///
/// Queries the daemon for its current control port and auth token, then
/// writes them to `/cella/.daemon_addr` on the volume so agents can
/// discover the daemon on startup and reconnect after restarts.
async fn write_daemon_addr_to_volume(client: &dyn ContainerBackend) {
    let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
        return;
    };
    if !mgmt_sock.exists() {
        return;
    }

    let Ok(cella_port::protocol::ManagementResponse::Status {
        control_port,
        control_token,
        ..
    }) = cella_daemon::management::send_management_request(
        &mgmt_sock,
        &cella_port::protocol::ManagementRequest::QueryStatus,
    )
    .await
    else {
        warn!("Failed to query daemon status for .daemon_addr write");
        return;
    };

    let gateway = client.host_gateway();
    let addr = format!("{gateway}:{control_port}");
    if let Err(e) = client.write_agent_addr("", &addr, &control_token).await {
        warn!("Failed to write .daemon_addr to agent volume: {e}");
    }
}

/// Kill the running cella-agent and restart it using the stable symlink.
///
/// Used after volume repopulation or daemon restart to ensure the agent
/// connects to the current daemon with the latest binary.
async fn restart_agent_in_container(client: &dyn ContainerBackend, container_id: &str) {
    let agent_path = "/cella/bin/cella-agent";
    let script = format!(
        "pkill -f 'cella-agent daemon' 2>/dev/null; \
         \"{agent_path}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" &"
    );

    match client
        .exec_detached(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), script],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        Ok(_) => info!("Agent restarted in container {container_id}"),
        Err(e) => warn!("Failed to restart agent in container: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── map_env_object ─────────────────────────────────────────────

    #[test]
    fn map_env_object_none() {
        let result = map_env_object(None);
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_null_value() {
        let val = serde_json::Value::Null;
        let result = map_env_object(Some(&val));
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_with_entries() {
        let val = serde_json::json!({
            "FOO": "bar",
            "BAZ": "qux"
        });
        let result = map_env_object(Some(&val));
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"FOO=bar".to_string()));
        assert!(result.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn map_env_object_empty_object() {
        let val = serde_json::json!({});
        let result = map_env_object(Some(&val));
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_with_null_values() {
        let val = serde_json::json!({
            "FOO": "bar",
            "SKIP": null
        });
        let result = map_env_object(Some(&val));
        // null values are typically filtered out
        assert!(result.iter().any(|e| e.starts_with("FOO=")));
    }

    // ── output_result ──────────────────────────────────────────────

    #[test]
    fn output_result_text_mode_does_not_panic() {
        // Text mode writes to stderr, just verify it doesn't panic
        output_result(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
        );
    }

    #[test]
    fn output_result_json_mode_does_not_panic() {
        // JSON mode writes to stdout, just verify it doesn't panic
        output_result(
            &OutputFormat::Json,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
        );
    }

    // ── resolve_remote_user ────────────────────────────────────────

    #[test]
    fn resolve_remote_user_from_config() {
        let config = serde_json::json!({
            "remoteUser": "devuser"
        });
        let user = resolve_remote_user(&config, None, "root");
        assert_eq!(user, "devuser");
    }

    #[test]
    fn resolve_remote_user_container_user_fallback() {
        let config = serde_json::json!({
            "containerUser": "containeruser"
        });
        let user = resolve_remote_user(&config, None, "root");
        assert_eq!(user, "containeruser");
    }

    #[test]
    fn resolve_remote_user_fallback_to_default() {
        let config = serde_json::json!({});
        let user = resolve_remote_user(&config, None, "root");
        assert_eq!(user, "root");
    }

    #[test]
    fn resolve_remote_user_remote_user_takes_priority() {
        let config = serde_json::json!({
            "remoteUser": "remote",
            "containerUser": "container"
        });
        let user = resolve_remote_user(&config, None, "root");
        assert_eq!(user, "remote");
    }

    // ── UpArgs::is_text_output ─────────────────────────────────────

    #[test]
    fn up_args_text_output() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "up"]).unwrap();
        if let crate::commands::Command::Up(args) = &cli.command {
            assert!(args.is_text_output());
        }
    }

    #[test]
    fn up_args_json_output() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "up", "--output", "json"]).unwrap();
        if let crate::commands::Command::Up(args) = &cli.command {
            assert!(!args.is_text_output());
        }
    }
}
