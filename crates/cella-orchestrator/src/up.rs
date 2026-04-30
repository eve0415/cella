//! Core "ensure container is up" pipeline.

use std::future::Future;
use std::pin::Pin;

use tracing::{debug, info, warn};

use cella_backend::agent::restart_agent_in_container;
use cella_backend::{
    BackendError, ContainerBackend, ContainerInfo, ContainerState, ExecOptions, ImageDetails,
    LifecycleContext, MountConfig, agent_env_vars, container_labels,
};

use crate::config::{HostRequirementPolicy, ImageStrategy, NetworkRulePolicy, UpConfig};
use crate::error::OrchestratorError;
use crate::lifecycle::{
    LifecycleState, WaitForPhase, check_and_run_content_update, lifecycle_entries_for_phase,
    read_lifecycle_state, run_lifecycle_entries, run_lifecycle_phases_with_wait_for,
    write_content_hash, write_lifecycle_state,
};
use crate::progress::ProgressSender;
use crate::result::{UpOutcome, UpResult};

/// Callbacks for host-specific actions during container up.
pub trait UpHooks: Send + Sync {
    /// Query daemon connection environment to inject into new containers.
    fn daemon_env<'a>(
        &'a self,
        container_name: &'a str,
        host_gateway: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>;

    /// Synchronize daemon connection details into managed agent storage.
    fn sync_agent_runtime<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Called after the container is running and network-ready.
    fn on_container_started(
        &self,
        container_id: &str,
        container_name: &str,
        container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Update a container's IP address with the daemon after pre-registration.
    /// Returns `true` if the container was found, `false` if unknown (e.g.
    /// daemon restarted and lost state — caller should fall back to full
    /// registration via `on_container_started`).
    fn update_container_ip(
        &self,
        container_id: &str,
        container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;

    /// Called before stopping or removing a managed container.
    fn on_container_stopping(
        &self,
        container_name: &str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// No-op hooks for contexts that don't need host integration.
pub struct NoOpHooks;

impl UpHooks for NoOpHooks {
    fn daemon_env<'a>(
        &'a self,
        _container_name: &'a str,
        _host_gateway: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async { Vec::new() })
    }

    fn sync_agent_runtime<'a>(
        &'a self,
        _client: &'a dyn ContainerBackend,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn on_container_started(
        &self,
        _container_id: &str,
        _container_name: &str,
        _container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }

    fn update_container_ip(
        &self,
        _container_id: &str,
        _container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { true })
    }

    fn on_container_stopping(
        &self,
        _container_name: &str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

struct EnsureUpContext<'a> {
    client: &'a dyn ContainerBackend,
    config: &'a UpConfig<'a>,
    hooks: &'a dyn UpHooks,
    progress: ProgressSender,
}

struct ImageConfig {
    image_env: Vec<String>,
    remote_user: String,
    env_fwd: cella_env::EnvForwarding,
    create_opts: cella_backend::CreateContainerOptions,
}

struct CreateResult {
    container_id: String,
    remote_user: String,
    ssh_agent_proxy: Option<crate::result::SshAgentProxyStatus>,
}

/// Remove the current SSH agent mount, env var, and label entry from
/// container create options. Also clears `env_fwd` so downstream label
/// readers (`cella exec`, `cella shell`) don't inject a stale socket path.
fn remove_ssh_from_create_opts(
    create_opts: &mut cella_backend::CreateContainerOptions,
    env_fwd: &mut cella_env::EnvForwarding,
) {
    if let Some(ref source) = env_fwd.ssh_agent_mount_source {
        create_opts.mounts.retain(|m| m.source != *source);
        env_fwd.mounts.retain(|m| m.source != *source);
    }
    create_opts.env.retain(|e| !e.starts_with("SSH_AUTH_SOCK="));
    env_fwd.env.retain(|e| e.key != "SSH_AUTH_SOCK");

    if let Some(label) = create_opts.labels.get_mut("dev.cella.remote_env")
        && let Ok(mut entries) = serde_json::from_str::<Vec<String>>(label)
    {
        entries.retain(|e| !e.starts_with("SSH_AUTH_SOCK="));
        *label = serde_json::to_string(&entries).unwrap_or_default();
    }

    env_fwd.ssh_agent_mount_source = None;
}

async fn run_step_result<F, T, E>(progress: &ProgressSender, label: &str, future: F) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let step = progress.step(label);
    match future.await {
        Ok(value) => {
            step.finish();
            Ok(value)
        }
        Err(error) => {
            step.fail(&error.to_string());
            Err(error)
        }
    }
}

impl EnsureUpContext<'_> {
    const fn config_json(&self) -> &serde_json::Value {
        &self.config.resolved.config
    }

    fn workspace_folder_str(&self) -> &str {
        self.config
            .workspace_folder_from_config
            .unwrap_or(self.config.default_workspace_folder)
    }

    fn probe_type(&self) -> &str {
        self.config_json()
            .get("userEnvProbe")
            .and_then(|v| v.as_str())
            .unwrap_or("loginInteractiveShell")
    }

    fn build_lifecycle_ctx<'b>(
        &'b self,
        container_id: &'b str,
        user: &'b str,
        env: &'b [String],
    ) -> LifecycleContext<'b> {
        let progress = self.progress.clone();
        LifecycleContext {
            client: self.client,
            container_id,
            user: Some(user),
            env,
            working_dir: Some(
                self.config
                    .workspace_folder_from_config
                    .unwrap_or(self.config.default_workspace_folder),
            ),
            is_text: true,
            on_output: Some(Box::new(move |line| progress.println(line))),
        }
    }

    async fn resolve_remote_user_from_container(&self, container: &ContainerInfo) -> String {
        if let Some(u) = self.config.resolved.remote_user() {
            return u.to_string();
        }
        if let Some(u) = self.config.resolved.container_user() {
            return u.to_string();
        }

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

    async fn detect_arch(&self) -> String {
        self.client
            .detect_container_arch()
            .await
            .unwrap_or_else(|e| {
                warn!("Container arch detection failed, defaulting to x86_64: {e}");
                "x86_64".to_string()
            })
    }

    async fn prepare_container_env(
        &self,
        container_id: &str,
        remote_user: &str,
    ) -> Result<
        (
            Option<std::collections::HashMap<String, String>>,
            Vec<String>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let config = self.config_json();

        let mut env_fwd = cella_env::prepare_env_forwarding(config, remote_user, None);
        if !self.client.capabilities().managed_agent {
            env_fwd
                .post_start
                .git_config_commands
                .retain(|cmd| !cmd.iter().any(|s| s.contains("cella-agent")));
        }
        let _ = run_step_result(&self.progress, "Configuring environment...", async {
            crate::container_setup::inject_post_start(
                self.client,
                container_id,
                &env_fwd.post_start,
                remote_user,
            )
            .await;
            Ok::<(), std::convert::Infallible>(())
        })
        .await;

        let shell = crate::shell_detect::detect_shell(self.client, container_id, remote_user).await;

        let probed_env = run_step_result(&self.progress, "Running userEnvProbe...", async {
            Ok::<_, std::convert::Infallible>(
                crate::env_cache::probe_and_cache_user_env(
                    self.client,
                    container_id,
                    remote_user,
                    self.probe_type(),
                    &shell,
                )
                .await,
            )
        })
        .await
        .ok()
        .flatten();

        let settings = cella_config::CellaConfig::load(
            &self.config.resolved.workspace_root,
            Some(self.config.resolved),
        )?;
        if settings.tools.claude_code.forward_config {
            crate::tool_install::create_claude_home_symlink(self.client, container_id, remote_user)
                .await;
            crate::tool_install::setup_plugin_manifests(self.client, container_id, remote_user)
                .await;
        }

        let any_tool = settings.tools.claude_code.enabled
            || settings.tools.codex.enabled
            || settings.tools.gemini.enabled;
        crate::tool_install::install_tools(
            self.client,
            container_id,
            remote_user,
            &settings,
            probed_env.as_ref(),
            &self.progress,
        )
        .await;

        let final_probed = if any_tool {
            run_step_result(&self.progress, "Updating environment cache...", async {
                Ok::<_, std::convert::Infallible>(
                    crate::env_cache::probe_and_cache_user_env(
                        self.client,
                        container_id,
                        remote_user,
                        self.probe_type(),
                        &shell,
                    )
                    .await
                    .or_else(|| probed_env.clone()),
                )
            })
            .await
            .ok()
            .flatten()
        } else {
            probed_env
        };

        let lifecycle_env = final_probed.as_ref().map_or_else(
            || self.config.remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, self.config.remote_env),
        );

        Ok((final_probed, lifecycle_env))
    }

    /// For prebuilt images: check lifecycle state and run `onCreateCommand` if
    /// it hasn't been completed yet. Updates the persisted state on success.
    async fn run_prebuilt_oncreate_if_needed(
        &self,
        container_id: &str,
        remote_user: &str,
        metadata: &str,
        lifecycle_env: &[String],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let lc_state = read_lifecycle_state(self.client, container_id, remote_user).await;
        if lc_state.oncreate_done {
            return Ok(());
        }
        let lc_ctx = self.build_lifecycle_ctx(container_id, remote_user, lifecycle_env);
        let entries =
            lifecycle_entries_for_phase(Some(metadata), self.config_json(), "onCreateCommand");
        run_lifecycle_entries(&lc_ctx, "onCreateCommand", &entries, &self.progress).await?;
        let new_state = LifecycleState {
            oncreate_done: true,
        };
        write_lifecycle_state(self.client, container_id, remote_user, &new_state).await;
        Ok(())
    }

    async fn handle_running(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<UpResult, Box<dyn std::error::Error + Send + Sync>> {
        // Spec: initializeCommand runs on the host during every start, including reconnects.
        let config = self.config_json();
        if let Some(init_cmd) = config.get("initializeCommand") {
            crate::container_setup::run_host_command("initializeCommand", init_cmd)?;
        }

        let capabilities = self.client.capabilities();

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

        if capabilities.managed_agent {
            self.hooks.sync_agent_runtime(self.client).await;
        }

        let container_version = container
            .labels
            .get("dev.cella.version")
            .map_or("unknown", String::as_str);
        let current_version = env!("CARGO_PKG_VERSION");
        if capabilities.managed_agent && container_version != current_version {
            info!(
                "Version change detected ({container_version} -> {current_version}), updating agent"
            );
            let agent_arch = self.detect_arch().await;
            if let Err(e) = self
                .client
                .ensure_agent_provisioned(current_version, &agent_arch, self.config.skip_checksum)
                .await
            {
                warn!("Failed to repopulate agent volume: {e}");
            }
        }

        if capabilities.managed_agent {
            self.ensure_agent_registered(&container.id).await;
        }

        let (_probed_env, lifecycle_env) = self
            .prepare_container_env(&container.id, remote_user)
            .await?;

        let metadata = container.labels.get("devcontainer.metadata");

        if let Some(meta) = metadata {
            self.run_prebuilt_oncreate_if_needed(&container.id, remote_user, meta, &lifecycle_env)
                .await?;
        }

        let lc_ctx_content = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        check_and_run_content_update(
            &lc_ctx_content,
            self.config_json(),
            metadata.map(String::as_str),
            &self.config.resolved.workspace_root,
            &self.progress,
        )
        .await?;

        let entries = lifecycle_entries_for_phase(
            metadata.map(String::as_str),
            self.config_json(),
            "postAttachCommand",
        );
        let lc_ctx = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        run_lifecycle_entries(&lc_ctx, "postAttachCommand", &entries, &self.progress).await?;

        Ok(UpResult {
            container_id: container.id.clone(),
            container_name: self.config.container_name.to_string(),
            remote_user: remote_user.to_string(),
            workspace_folder: self.workspace_folder_str().to_string(),
            outcome: UpOutcome::Running,
            ssh_agent_proxy: None,
        })
    }

    /// Run the restart lifecycle after a stopped container has been started:
    /// check prebuilt oncreate, content update, then run postStart + postAttach phases.
    async fn run_restart_lifecycle(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_probed_env, lifecycle_env) = self
            .prepare_container_env(&container.id, remote_user)
            .await?;
        let metadata = container.labels.get("devcontainer.metadata");

        if let Some(meta) = metadata {
            self.run_prebuilt_oncreate_if_needed(&container.id, remote_user, meta, &lifecycle_env)
                .await?;
        }

        let lc_ctx = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        check_and_run_content_update(
            &lc_ctx,
            self.config_json(),
            metadata.map(String::as_str),
            &self.config.resolved.workspace_root,
            &self.progress,
        )
        .await?;

        for phase in ["postStartCommand", "postAttachCommand"] {
            let entries = lifecycle_entries_for_phase(
                metadata.map(String::as_str),
                self.config_json(),
                phase,
            );
            run_lifecycle_entries(&lc_ctx, phase, &entries, &self.progress).await?;
        }
        Ok(())
    }

    /// Emit warnings when the container's config or runtime has drifted.
    fn warn_config_drift(&self, container: &ContainerInfo) {
        if let Some(old_hash) = &container.config_hash
            && *old_hash != self.config.resolved.config_hash
        {
            self.progress
                .warn("Config has changed since this container was created.");
            self.progress
                .hint("Run `cella up --rebuild` to recreate with the updated config.");
        }

        let current_runtime = cella_env::platform::detect_runtime();
        if let Some(old_runtime) = container.labels.get("dev.cella.docker_runtime") {
            let current_label = current_runtime.as_label();
            if old_runtime != current_label {
                self.progress.warn(&format!(
                    "Docker runtime changed ({old_runtime} -> {current_label})."
                ));
                self.progress
                    .hint("Run `cella up --rebuild` to recreate with the updated runtime.");
            }
        }
    }

    /// Restart the agent and ensure the container is registered with the daemon.
    ///
    /// Restarts the agent so it picks up the latest `.daemon_addr`, then
    /// tries a non-destructive IP update. If the daemon doesn't know about
    /// this container (e.g. after a daemon restart), falls back to full
    /// registration.
    async fn ensure_agent_registered(&self, container_id: &str) {
        restart_agent_in_container(self.client, container_id).await;

        let container_ip = self
            .client
            .get_container_ip(container_id)
            .await
            .unwrap_or(None);
        let known = self
            .hooks
            .update_container_ip(container_id, container_ip.as_deref())
            .await;
        if !known {
            info!("Container not registered with daemon, performing full registration");
            self.hooks
                .on_container_started(
                    container_id,
                    self.config.container_name,
                    container_ip.as_deref(),
                )
                .await;
        }
    }

    /// Roll back a daemon pre-registration that was made before a failed start.
    async fn rollback_preregistration(&self) {
        if self.client.capabilities().managed_agent {
            self.hooks
                .on_container_stopping(self.config.container_name)
                .await;
        }
    }

    async fn handle_stopped(
        &self,
        container: &ContainerInfo,
        remote_user: &str,
    ) -> Result<Option<UpResult>, Box<dyn std::error::Error + Send + Sync>> {
        let capabilities = self.client.capabilities();

        self.warn_config_drift(container);

        if capabilities.managed_agent {
            let agent_arch = self.detect_arch().await;
            run_step_result(
                &self.progress,
                "Populating agent volume...",
                self.client.ensure_agent_provisioned(
                    env!("CARGO_PKG_VERSION"),
                    &agent_arch,
                    self.config.skip_checksum,
                ),
            )
            .await?;
            self.hooks.sync_agent_runtime(self.client).await;
            // Pre-register before start so the agent can report ports immediately.
            self.hooks
                .on_container_started(&container.id, self.config.container_name, None)
                .await;
        }

        // Don't re-register here. The container's CELLA_SSH_AGENT_BRIDGE
        // env var was baked at create time and can't be updated, so a
        // fresh register on a new port wouldn't help. The daemon reclaims
        // bridge ports from its state file on startup instead (see
        // SshProxyManager::reclaim_from_state_file), which typically
        // lets the stopped-container restart path work transparently.
        // If the daemon's reclaim fails (port taken by something else,
        // stale state file, etc.), recovery requires `cella down &&
        // cella up`.
        let ssh_agent_proxy: Option<crate::result::SshAgentProxyStatus> = None;

        let step = self.progress.step("Starting container...");
        let start_result = self.client.start_container(&container.id).await;
        if start_result.is_ok() {
            step.finish();
        } else {
            step.fail("failed");
        }

        match start_result {
            Ok(()) => {
                if let Err(e) =
                    crate::container_setup::verify_container_running(self.client, &container.id)
                        .await
                {
                    self.rollback_preregistration().await;
                    return Err(e);
                }

                let container_ip = self
                    .client
                    .get_container_ip(&container.id)
                    .await
                    .unwrap_or(None);

                if capabilities.managed_agent {
                    // Full re-registration with the actual IP. This is the
                    // authoritative registration — the pre-registration before
                    // start was best-effort so the agent doesn't race.
                    // register_container() releases old state, so this is safe
                    // even if the pre-registration succeeded.
                    self.hooks
                        .on_container_started(
                            &container.id,
                            self.config.container_name,
                            container_ip.as_deref(),
                        )
                        .await;
                    restart_agent_in_container(self.client, &container.id).await;
                }

                self.run_restart_lifecycle(container, remote_user).await?;

                if capabilities.managed_agent
                    && let Err(e) = self
                        .client
                        .prune_old_agent_versions(env!("CARGO_PKG_VERSION"))
                        .await
                {
                    debug!("Agent version pruning failed: {e}");
                }

                Ok(Some(UpResult {
                    container_id: container.id.clone(),
                    container_name: self.config.container_name.to_string(),
                    remote_user: remote_user.to_string(),
                    workspace_folder: self.workspace_folder_str().to_string(),
                    outcome: UpOutcome::Started,
                    ssh_agent_proxy,
                }))
            }
            Err(e) => {
                warn!("Failed to start existing container: {e}");
                self.rollback_preregistration().await;
                self.progress
                    .warn(&format!("Could not start existing container: {e}"));
                self.progress.hint("Recreating container...");
                let _ = self.client.remove_container(&container.id, false).await;
                Ok(None)
            }
        }
    }

    async fn remove_existing(
        &self,
        container: &ContainerInfo,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.hooks
            .on_container_stopping(self.config.container_name)
            .await;
        if container.state == ContainerState::Running {
            info!("Stopping container for {reason}...");
            self.client.stop_container(&container.id).await?;
        }
        self.client.remove_container(&container.id, false).await?;
        Ok(())
    }

    fn build_labels(
        &self,
        resolved_features: Option<&cella_features::ResolvedFeatures>,
        base_metadata: Option<&str>,
        env_fwd: &cella_env::EnvForwarding,
        remote_user: &str,
    ) -> std::collections::HashMap<String, String> {
        let config = self.config_json();
        let runtime = cella_env::platform::detect_runtime();
        let mut labels = container_labels(
            &self.config.resolved.workspace_root,
            &self.config.resolved.config_path,
            &self.config.resolved.config_hash,
            runtime.as_label(),
        );

        labels.insert(
            cella_backend::BACKEND_LABEL.to_string(),
            self.client.kind().to_string(),
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

        let mut label_remote_env = self.config.remote_env.to_vec();
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
        } else if let Some(existing) = base_metadata {
            // The base image already carries a devcontainer.metadata label
            // (either from a prebuilt image or from the Dockerfile build that
            // embedded it via --label). Re-use it directly instead of
            // regenerating, which would duplicate entries.
            labels.insert("devcontainer.metadata".to_string(), existing.to_string());
        } else {
            // No features and no base image metadata -- generate from config.
            labels.insert(
                "devcontainer.metadata".to_string(),
                cella_features::generate_metadata_label(&[], config, None),
            );
        }

        let ports_attrs = crate::config_map::ports::parse_ports_attributes(config);
        let other_ports_attrs = crate::config_map::ports::parse_other_ports_attributes(config);
        labels.insert(
            "dev.cella.ports_attributes".to_string(),
            crate::config_map::ports::serialize_ports_attributes_label(
                &ports_attrs,
                other_ports_attrs.as_ref(),
            ),
        );

        if let Some(action) = config.get("shutdownAction").and_then(|v| v.as_str()) {
            labels.insert("dev.cella.shutdown_action".to_string(), action.to_string());
        }

        labels.extend(self.config.extra_labels.clone());
        labels
    }

    /// If `env_fwd` carries a deferred colima SSH-agent proxy request,
    /// resolve it via the daemon and append the resulting bind-mount and
    /// `SSH_AUTH_SOCK` env entries. Returns the status for the CLI to
    /// render. `None` means the proxy code path was not exercised at all
    /// (no request from `cella-env`).
    async fn resolve_ssh_agent_proxy(
        &self,
        env_fwd: &mut cella_env::EnvForwarding,
    ) -> Option<crate::result::SshAgentProxyStatus> {
        let request = env_fwd.ssh_agent_proxy_request.take()?;
        let Some(daemon_sock) = cella_env::paths::daemon_socket_path() else {
            return Some(crate::result::SshAgentProxyStatus::Skipped {
                reason: "daemon socket path could not be determined".to_string(),
            });
        };
        let host_gateway = self.client.host_gateway();
        match crate::ssh_proxy_client::register_proxy(
            &daemon_sock,
            &self.config.resolved.workspace_root,
            host_gateway,
            &request,
        )
        .await
        {
            Some(resolved) => {
                env_fwd.env.extend(resolved.env);
                Some(crate::result::SshAgentProxyStatus::Bridged {
                    host_endpoint: format!("{host_gateway}:{}", resolved.bridge_port),
                    refcount: resolved.refcount,
                })
            }
            None => Some(crate::result::SshAgentProxyStatus::Skipped {
                reason: "daemon RegisterSshAgentProxy failed (see daemon log)".to_string(),
            }),
        }
    }

    async fn apply_env_and_mounts(
        &self,
        create_opts: &mut cella_backend::CreateContainerOptions,
        env_fwd: &cella_env::EnvForwarding,
        image_env: &[String],
        remote_user: &str,
        settings: &cella_config::CellaConfig,
        agent_arch: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let capabilities = self.client.capabilities();

        for m in &env_fwd.mounts {
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: m.source.clone(),
                target: m.target.clone(),
                consistency: None,
                read_only: false,
            });
        }

        for spec in crate::tool_install::build_tool_config_mount_specs(settings, remote_user) {
            create_opts.mounts.push(spec.to_mount_config());
        }

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

        if capabilities.managed_agent {
            let daemon_env = self
                .hooks
                .daemon_env(self.config.container_name, self.client.host_gateway())
                .await;
            if !daemon_env.is_empty() {
                if create_opts.env.is_empty() {
                    create_opts.env = image_env.to_vec();
                }
                create_opts.env.extend(daemon_env);
            }
        }

        if capabilities.managed_agent {
            let version = env!("CARGO_PKG_VERSION");
            let agent_step = self.progress.step("Populating agent volume...");
            match self
                .client
                .ensure_agent_provisioned(version, agent_arch, self.config.skip_checksum)
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

            self.hooks.sync_agent_runtime(self.client).await;

            if create_opts.env.is_empty() {
                create_opts.env = image_env.to_vec();
            }
            create_opts.env.extend(agent_env_vars());
        } else {
            self.progress.warn(
                "Selected backend does not support managed agent provisioning; port forwarding and BROWSER interception are disabled.",
            );
        }

        if let Some(parent_git) = cella_git::parent_git_dir(&self.config.resolved.workspace_root) {
            let canonical = parent_git
                .canonicalize()
                .unwrap_or_else(|_| parent_git.clone());
            let path_str = canonical.to_string_lossy().to_string();
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: path_str.clone(),
                target: path_str,
                consistency: None,
                read_only: false,
            });
        }

        let (vol_name, vol_target, _ro) = self.client.agent_volume_mount();
        if capabilities.managed_agent && !vol_name.is_empty() {
            create_opts.mounts.push(MountConfig {
                mount_type: "volume".to_string(),
                source: vol_name,
                target: vol_target,
                consistency: None,
                read_only: false,
            });
        }

        Ok(())
    }

    async fn start_and_notify(
        &self,
        container_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let label = if self.progress.is_verbose() {
            let short_id = &container_id[..12.min(container_id.len())];
            format!("Starting container: {short_id}...")
        } else {
            "Starting container...".to_string()
        };
        run_step_result(
            &self.progress,
            &label,
            self.client.start_container(container_id),
        )
        .await?;
        crate::container_setup::verify_container_running(self.client, container_id).await?;

        if let Err(e) = self
            .client
            .ensure_container_network(container_id, &self.config.resolved.workspace_root)
            .await
        {
            warn!("Failed to connect container to networks: {e}");
        }

        for net in &self.config.extra_networks {
            if let Err(e) = self.client.connect_to_network(container_id, net).await {
                warn!("Failed to connect container to extra network '{net}': {e}");
            }
        }

        let container_ip = self
            .client
            .get_container_ip(container_id)
            .await
            .unwrap_or(None);
        self.hooks
            .on_container_started(
                container_id,
                self.config.container_name,
                container_ip.as_deref(),
            )
            .await;
        Ok(())
    }

    async fn post_create_setup(
        &self,
        container_id: &str,
        remote_user: &str,
        env_fwd: &cella_env::EnvForwarding,
        settings: &cella_config::CellaConfig,
        remote_env: &[String],
    ) -> (
        Option<std::collections::HashMap<String, String>>,
        Vec<String>,
    ) {
        let _ = run_step_result(&self.progress, "Configuring environment...", async {
            crate::container_setup::inject_post_start(
                self.client,
                container_id,
                &env_fwd.post_start,
                remote_user,
            )
            .await;
            Ok::<(), std::convert::Infallible>(())
        })
        .await;

        // The container's entrypoint launches `cella-agent daemon` immediately,
        // before this post-start step uploads `proxy-config.json`. The daemon
        // therefore reads `CELLA_PROXY_CONFIG`, finds no file, and gives up
        // without binding port 18080 — leaving every connection through
        // `HTTP_PROXY` to ECONNREFUSED. Restart the daemon now that the file
        // exists so the entrypoint's restart loop respawns it against a
        // populated config.
        if injected_proxy_config(&env_fwd.post_start) && self.client.capabilities().managed_agent {
            restart_agent_in_container(self.client, container_id).await;
        }

        crate::container_setup::inject_cella_path(self.client, container_id, remote_user).await;

        if settings.credentials.gh {
            crate::container_setup::seed_gh_credentials(
                self.client,
                container_id,
                &self.config.resolved.workspace_root,
                remote_user,
            )
            .await;
        }

        // Log detected AI API keys (names only, never values)
        if settings.credentials.ai.enabled {
            let ai = &settings.credentials.ai;
            let detected =
                cella_env::ai_keys::detect_ai_key_names(&|id| ai.is_provider_enabled(id));
            if !detected.is_empty() {
                tracing::debug!(
                    "AI API keys detected for exec/shell forwarding: {}",
                    detected.join(", ")
                );
            }
        }

        let shell = crate::shell_detect::detect_shell(self.client, container_id, remote_user).await;
        let probed_env = run_step_result(&self.progress, "Running userEnvProbe...", async {
            Ok::<_, std::convert::Infallible>(
                crate::env_cache::probe_and_cache_user_env(
                    self.client,
                    container_id,
                    remote_user,
                    self.probe_type(),
                    &shell,
                )
                .await,
            )
        })
        .await
        .ok()
        .flatten();

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

        self.install_tools_and_probe_env(
            container_id,
            remote_user,
            settings,
            &shell,
            probed_env,
            remote_env,
        )
        .await
    }

    async fn install_tools_and_probe_env(
        &self,
        container_id: &str,
        remote_user: &str,
        settings: &cella_config::CellaConfig,
        shell: &str,
        probed_env: Option<std::collections::HashMap<String, String>>,
        remote_env: &[String],
    ) -> (
        Option<std::collections::HashMap<String, String>>,
        Vec<String>,
    ) {
        if settings.tools.claude_code.forward_config {
            crate::tool_install::create_claude_home_symlink(self.client, container_id, remote_user)
                .await;
            crate::tool_install::setup_plugin_manifests(self.client, container_id, remote_user)
                .await;
        }

        let any_tool = settings.tools.claude_code.enabled
            || settings.tools.codex.enabled
            || settings.tools.gemini.enabled;
        crate::tool_install::install_tools(
            self.client,
            container_id,
            remote_user,
            settings,
            probed_env.as_ref(),
            &self.progress,
        )
        .await;

        let final_probed = if any_tool {
            run_step_result(&self.progress, "Updating environment cache...", async {
                Ok::<_, std::convert::Infallible>(
                    crate::env_cache::probe_and_cache_user_env(
                        self.client,
                        container_id,
                        remote_user,
                        self.probe_type(),
                        shell,
                    )
                    .await
                    .or_else(|| probed_env.clone()),
                )
            })
            .await
            .ok()
            .flatten()
        } else {
            probed_env
        };

        let lifecycle_env = final_probed.as_ref().map_or_else(
            || remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, remote_env),
        );

        (final_probed, lifecycle_env)
    }

    fn resolve_image_config(
        &self,
        img_name: &str,
        base_image_details: ImageDetails,
        resolved_features: Option<&cella_features::ResolvedFeatures>,
        agent_arch: &str,
    ) -> Result<ImageConfig, Box<dyn std::error::Error + Send + Sync>> {
        let config = self.config_json();
        let image_env = base_image_details.env;
        let image_meta_user = base_image_details
            .metadata
            .as_deref()
            .map(|m| cella_features::parse_image_metadata(m).1);
        let remote_user = crate::container_setup::resolve_remote_user(
            config,
            image_meta_user.as_ref(),
            &base_image_details.user,
        );

        let settings = cella_config::CellaConfig::load(
            &self.config.resolved.workspace_root,
            Some(self.config.resolved),
        )?;
        let net_config = settings.network.to_network_config();
        let skip_rules = self.config.network_rule_policy == NetworkRulePolicy::Skip;
        let has_rules = net_config.has_rules() && !skip_rules;

        // For unmanaged backends, still forward upstream proxy env vars for
        // direct passthrough, but disable blocking rules (which require the
        // agent-side proxy that won't be provisioned).
        let managed_agent = self.client.capabilities().managed_agent;
        let proxy_fwd = Some(cella_env::ProxyForwardingConfig {
            proxy: net_config.proxy.clone(),
            has_blocking_rules: has_rules && managed_agent,
            full_config: if has_rules && managed_agent {
                Some(net_config)
            } else {
                None
            },
            container_distro: cella_env::ca_bundle::ContainerDistro::Unknown,
        });
        let mut env_fwd =
            cella_env::prepare_env_forwarding(config, &remote_user, proxy_fwd.as_ref());

        // Strip agent-dependent credential helper for unmanaged backends —
        // the cella-agent binary won't be provisioned, so the helper would
        // fail with "not found" on every git credential request.
        if !managed_agent {
            env_fwd
                .post_start
                .git_config_commands
                .retain(|cmd| !cmd.iter().any(|s| s.contains("cella-agent")));
        }

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

        let create_opts = crate::config_map::map_config(crate::config_map::MapConfigParams {
            config,
            container_name: self.config.container_name,
            image_name: img_name,
            labels,
            workspace_root: &self.config.resolved.workspace_root,
            feature_config: effective_feature_config,
            image_env: &image_env,
            agent_arch,
        });

        Ok(ImageConfig {
            image_env,
            remote_user,
            env_fwd,
            create_opts,
        })
    }

    /// Run lifecycle phases and write tracking state after container creation.
    async fn run_create_lifecycle(
        &self,
        container_id: &str,
        remote_user: &str,
        lifecycle_env: &[String],
        resolved_features: Option<&cella_features::ResolvedFeatures>,
        image_metadata: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config = self.config_json();
        let wait_for = WaitForPhase::from_config(config);
        let lc_ctx = self.build_lifecycle_ctx(container_id, remote_user, lifecycle_env);
        run_lifecycle_phases_with_wait_for(
            &lc_ctx,
            config,
            resolved_features,
            image_metadata,
            &self.progress,
            wait_for,
        )
        .await?;

        write_content_hash(
            self.client,
            container_id,
            remote_user,
            &self.config.resolved.workspace_root,
        )
        .await;

        // Mark onCreateCommand as done only when it actually ran in the
        // foreground. The phases array is [onCreate, updateContent, postCreate,
        // postStart, postAttach]. When waitFor is Initialize (ordinal 0), ALL
        // phases are backgrounded and the background script writes the state
        // file on completion. For any other waitFor value, onCreateCommand
        // (index 0) ran synchronously before we got here.
        let oncreate_foreground = !matches!(wait_for, WaitForPhase::Initialize);
        let state = LifecycleState {
            oncreate_done: oncreate_foreground,
        };
        write_lifecycle_state(self.client, container_id, remote_user, &state).await;

        Ok(())
    }

    /// Create a container with progress reporting.
    async fn create_container(
        &self,
        create_opts: &cella_backend::CreateContainerOptions,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if self.progress.is_verbose() {
            let step = self.progress.step(&format!(
                "Creating container: {}...",
                self.config.container_name
            ));
            let result = self.client.create_container(create_opts).await;
            match &result {
                Ok(_) => step.finish(),
                Err(e) => step.fail(&e.to_string()),
            }
            Ok(result?)
        } else {
            Ok(run_step_result(
                &self.progress,
                "Creating container...",
                self.client.create_container(create_opts),
            )
            .await?)
        }
    }

    /// Try creating the container. If it fails with an SSH agent bind-mount
    /// error, try fallback strategies, then skip SSH forwarding entirely.
    async fn create_container_with_ssh_fallback(
        &self,
        create_opts: &mut cella_backend::CreateContainerOptions,
        env_fwd: &mut cella_env::EnvForwarding,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        loop {
            match self.create_container(create_opts).await {
                Ok(id) => return Ok(id),
                Err(e) => {
                    let err_msg = e.to_string();
                    if !cella_env::ssh_agent::is_ssh_mount_error(
                        &err_msg,
                        env_fwd.ssh_agent_mount_source.as_deref(),
                    ) {
                        return Err(e);
                    }

                    remove_ssh_from_create_opts(create_opts, env_fwd);

                    if let Some(next) = env_fwd.ssh_agent_fallbacks.first().cloned() {
                        env_fwd.ssh_agent_fallbacks.remove(0);
                        match next {
                            cella_env::ssh_agent::SshAgentRequest::Direct(ssh) => {
                                info!(
                                    "SSH agent mount failed, trying fallback: {} -> {}",
                                    ssh.mount_source, ssh.mount_target
                                );
                                env_fwd.ssh_agent_mount_source = Some(ssh.mount_source.clone());
                                create_opts.mounts.push(MountConfig {
                                    mount_type: "bind".to_string(),
                                    source: ssh.mount_source,
                                    target: ssh.mount_target.clone(),
                                    consistency: None,
                                    read_only: false,
                                });
                                create_opts
                                    .env
                                    .push(format!("SSH_AUTH_SOCK={}", ssh.env_value));
                                continue;
                            }
                            cella_env::ssh_agent::SshAgentRequest::ProxyOnColima { .. } => {
                                debug!("Skipping ProxyOnColima fallback in retry loop");
                            }
                        }
                    }

                    let runtime = env_fwd
                        .ssh_agent_runtime
                        .as_ref()
                        .unwrap_or(&cella_env::DockerRuntime::Unknown);
                    self.progress
                        .warn(&cella_env::ssh_agent::ssh_skip_warning(runtime));
                    return self.create_container(create_opts).await;
                }
            }
        }
    }

    /// Build a UID-remapped image and update `create_opts.image` if needed.
    async fn maybe_remap_uid(
        &self,
        config: &serde_json::Value,
        img_name: &str,
        image_user: &str,
        remote_user: &str,
        create_opts: &mut cella_backend::CreateContainerOptions,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let update_uid = config
            .get("updateRemoteUserUID")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        if update_uid
            && let Some(uid_img) = crate::uid_image::build_uid_remap_image(
                self.client,
                img_name,
                image_user,
                remote_user,
                &self.progress,
            )
            .await?
        {
            create_opts.image = uid_img;
        }
        Ok(())
    }

    async fn create_and_start(
        &self,
        build_no_cache: bool,
    ) -> Result<CreateResult, Box<dyn std::error::Error + Send + Sync>> {
        let config = self.config_json();

        if let Some(init_cmd) = config.get("initializeCommand") {
            crate::container_setup::run_host_command("initializeCommand", init_cmd)?;
        }

        let (img_name, resolved_features, base_image_details) =
            crate::image::ensure_image(&crate::image::EnsureImageInput {
                client: self.client,
                config,
                workspace_root: &self.config.resolved.workspace_root,
                config_name: self.config.resolved.name(),
                config_path: &self.config.resolved.config_path,
                no_cache: build_no_cache,
                pull_policy: self.config.pull_policy,
                secrets: &self.config.build_secrets,
                progress: &self.progress,
            })
            .await?;

        let image_user = base_image_details.user.clone();
        let image_metadata = if resolved_features.is_none() {
            base_image_details.metadata.clone()
        } else {
            None
        };

        let agent_arch = self.detect_arch().await;
        let ImageConfig {
            image_env,
            remote_user,
            mut env_fwd,
            mut create_opts,
        } = self.resolve_image_config(
            &img_name,
            base_image_details,
            resolved_features.as_ref(),
            &agent_arch,
        )?;

        let ssh_agent_proxy = self.resolve_ssh_agent_proxy(&mut env_fwd).await;

        self.maybe_remap_uid(
            config,
            &img_name,
            &image_user,
            &remote_user,
            &mut create_opts,
        )
        .await?;

        let settings = cella_config::CellaConfig::load(
            &self.config.resolved.workspace_root,
            Some(self.config.resolved),
        )?;
        self.apply_env_and_mounts(
            &mut create_opts,
            &env_fwd,
            &image_env,
            &remote_user,
            &settings,
            &agent_arch,
        )
        .await?;

        let container_id = self
            .create_container_with_ssh_fallback(&mut create_opts, &mut env_fwd)
            .await?;

        self.start_and_notify(&container_id).await?;

        let (_probed_env, lifecycle_env) = self
            .post_create_setup(
                &container_id,
                &remote_user,
                &env_fwd,
                &settings,
                &create_opts.remote_env,
            )
            .await;

        let lifecycle_err = self
            .run_create_lifecycle(
                &container_id,
                &remote_user,
                &lifecycle_env,
                resolved_features.as_ref(),
                image_metadata.as_deref(),
            )
            .await
            .err()
            .map(|e| e.to_string());
        if let Some(msg) = lifecycle_err {
            warn!("Lifecycle failed, cleaning up container {container_id}");
            let _ = self.client.stop_container(&container_id).await;
            let _ = self.client.remove_container(&container_id, false).await;
            return Err(msg.into());
        }

        Ok(CreateResult {
            container_id,
            remote_user,
            ssh_agent_proxy,
        })
    }

    async fn ensure_up(self) -> Result<UpResult, Box<dyn std::error::Error + Send + Sync>> {
        let build_no_cache = matches!(self.config.image_strategy, ImageStrategy::RebuildNoCache);
        let remove_container = self.config.remove_existing_container
            || matches!(
                self.config.image_strategy,
                ImageStrategy::Rebuild | ImageStrategy::RebuildNoCache
            );

        if self.config_json().get("dockerComposeFile").is_some() {
            return Err(
                "Docker Compose projects are not yet supported with orchestrator::up::ensure_up"
                    .into(),
            );
        }

        let existing = self
            .client
            .find_container(&self.config.resolved.workspace_root)
            .await?;

        if let Some(container) = existing {
            let remote_user = self.resolve_remote_user_from_container(&container).await;

            match (&container.state, remove_container) {
                (ContainerState::Running, false) if !build_no_cache => {
                    return self.handle_running(&container, &remote_user).await;
                }
                (ContainerState::Stopped, false) => {
                    if let Some(result) = self.handle_stopped(&container, &remote_user).await? {
                        return Ok(result);
                    }
                }
                (ContainerState::Running, false) => {
                    self.remove_existing(&container, "--build-no-cache").await?;
                }
                (ContainerState::Running, true) => {
                    self.remove_existing(&container, "rebuild").await?;
                }
                (_, true) => {
                    if container.state == ContainerState::Running {
                        self.hooks
                            .on_container_stopping(self.config.container_name)
                            .await;
                        self.client.stop_container(&container.id).await?;
                    }
                    self.client.remove_container(&container.id, false).await?;
                }
                (_, false) => {
                    self.client.remove_container(&container.id, false).await?;
                }
            }
        }

        let create_result = self.create_and_start(build_no_cache).await?;
        Ok(UpResult {
            container_id: create_result.container_id,
            container_name: self.config.container_name.to_string(),
            remote_user: create_result.remote_user,
            workspace_folder: self.workspace_folder_str().to_string(),
            outcome: UpOutcome::Created,
            ssh_agent_proxy: create_result.ssh_agent_proxy,
        })
    }
}

/// Run the full non-compose container-up pipeline.
///
/// # Errors
///
/// Returns `OrchestratorError` when the container cannot be created, started,
/// or configured (e.g. image pull failure, host requirement violation).
pub async fn ensure_up(
    client: &dyn ContainerBackend,
    config: &UpConfig<'_>,
    hooks: &dyn UpHooks,
    progress: ProgressSender,
) -> Result<UpResult, OrchestratorError> {
    if config.resolved.config.get("hostRequirements").is_some() {
        let result = crate::host_requirements::validate(
            &config.resolved.config,
            &config.resolved.workspace_root,
        );
        for check in &result.checks {
            if !check.met {
                progress.warn(&format!(
                    "Host does not meet requirement: {} (need {}, have {})",
                    check.name, check.required, check.actual
                ));
            }
        }
        if !result.all_met && config.host_requirement_policy == HostRequirementPolicy::Error {
            return Err(OrchestratorError::HostRequirements {
                message: "Host does not meet hostRequirements".to_string(),
            });
        }
    }

    EnsureUpContext {
        client,
        config,
        hooks,
        progress,
    }
    .ensure_up()
    .await
    .map_err(|e| OrchestratorError::Other {
        message: e.to_string(),
    })
}

/// Returns `true` if the post-start injection includes the cella-agent
/// proxy config upload. Used by the create flow to decide whether to
/// restart the agent so it picks up the now-present config file.
fn injected_proxy_config(post_start: &cella_env::PostStartInjection) -> bool {
    post_start
        .file_uploads
        .iter()
        .any(|upload| upload.container_path == cella_env::PROXY_CONFIG_PATH)
}

/// Restart the in-container agent daemon.
///
/// Kills only the `cella-agent daemon` process (not credential or browser
/// helpers that exec the same binary), waits briefly for the entrypoint
/// restart loop to respawn it, and only explicitly starts a replacement
/// if nothing came back. This avoids double-launching the daemon on
/// containers with the restart loop while remaining backward compatible
/// with containers created before it was introduced.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_rule_policy_enforce_eq() {
        assert_eq!(NetworkRulePolicy::Enforce, NetworkRulePolicy::Enforce);
        assert_ne!(NetworkRulePolicy::Enforce, NetworkRulePolicy::Skip);
    }

    #[test]
    fn network_rule_policy_skip_eq() {
        assert_eq!(NetworkRulePolicy::Skip, NetworkRulePolicy::Skip);
    }

    #[test]
    fn injected_proxy_config_detects_upload() {
        let mut post_start = cella_env::PostStartInjection::default();
        post_start.file_uploads.push(cella_env::FileUpload {
            container_path: cella_env::PROXY_CONFIG_PATH.to_string(),
            content: b"{}".to_vec(),
            mode: 0o600,
        });
        assert!(injected_proxy_config(&post_start));
    }

    #[test]
    fn injected_proxy_config_false_when_only_other_uploads() {
        let mut post_start = cella_env::PostStartInjection::default();
        post_start.file_uploads.push(cella_env::FileUpload {
            container_path: "/etc/some-other-file".to_string(),
            content: b"x".to_vec(),
            mode: 0o644,
        });
        assert!(!injected_proxy_config(&post_start));
    }

    #[test]
    fn injected_proxy_config_false_when_empty() {
        let post_start = cella_env::PostStartInjection::default();
        assert!(!injected_proxy_config(&post_start));
    }

    #[tokio::test]
    async fn noop_hooks_are_noops() {
        let hooks = NoOpHooks;
        hooks
            .on_container_started("container-123", "test-container", Some("172.17.0.2"))
            .await;
        hooks.on_container_stopping("test-container").await;
        assert!(hooks.daemon_env("test", "host").await.is_empty());
        assert!(hooks.update_container_ip("container-123", None).await);
    }

    #[tokio::test]
    async fn run_step_result_emits_completed_event_on_success() {
        use crate::progress::{ProgressEvent, ProgressSender};

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgressEvent>(8);
        let progress = ProgressSender::new(tx, false);

        let value = run_step_result(&progress, "Doing work...", async {
            Ok::<_, std::convert::Infallible>("done")
        })
        .await
        .unwrap();

        assert_eq!(value, "done");
        let first = rx.try_recv().expect("phase started");
        let second = rx.try_recv().expect("phase completed");
        assert!(matches!(first, ProgressEvent::StepStarted { .. }));
        assert!(matches!(second, ProgressEvent::StepCompleted { .. }));
    }

    #[tokio::test]
    async fn run_step_result_emits_failed_event_on_error() {
        use crate::progress::{ProgressEvent, ProgressSender};

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgressEvent>(8);
        let progress = ProgressSender::new(tx, false);

        let result =
            run_step_result(&progress, "Doing work...", async { Err::<(), _>("boom") }).await;

        assert_eq!(result, Err("boom"));
        let first = rx.try_recv().expect("phase started");
        let second = rx.try_recv().expect("phase failed");
        assert!(matches!(first, ProgressEvent::StepStarted { .. }));
        assert!(matches!(
            second,
            ProgressEvent::StepFailed { message, .. } if message == "boom"
        ));
    }
}
