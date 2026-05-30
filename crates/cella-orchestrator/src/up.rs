//! Core "ensure container is up" pipeline.

use std::future::Future;
use std::pin::Pin;

use tracing::{debug, info, warn};

use cella_backend::agent::restart_agent_in_container;
use cella_backend::{
    BackendError, ContainerBackend, ContainerInfo, ContainerState, ExecOptions, FileToUpload,
    ImageDetails, LifecycleContext, MountConfig, agent_env_vars, container_labels,
};

use crate::config::{HostRequirementPolicy, ImageStrategy, NetworkRulePolicy, UpConfig};
use crate::error::OrchestratorError;
use cella_backend::EXPECTED_CONTAINER_MISSING;

use crate::lifecycle::{
    LifecycleState, check_and_run_content_update, lifecycle_entries_for_phase,
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

    /// Run the `userEnvProbe`, unless `--skip-post-create` gates it. Returns
    /// `None` when gated (official suppresses `probeRemoteEnv` under
    /// `postCreateEnabled=false`) or when the probe yields nothing.
    async fn probe_user_env_if_enabled(
        &self,
        container_id: &str,
        remote_user: &str,
        shell: &str,
    ) -> Option<std::collections::HashMap<String, String>> {
        if !self.config.lifecycle_gate.enabled {
            return None;
        }
        run_step_result(&self.progress, "Running userEnvProbe...", async {
            Ok::<_, std::convert::Infallible>(
                crate::env_cache::probe_and_cache_user_env(
                    self.client,
                    container_id,
                    remote_user,
                    self.config.user_env_probe,
                    shell,
                )
                .await,
            )
        })
        .await
        .ok()
        .flatten()
    }

    /// Re-run the probe after tool installation, falling back to the prior
    /// probe result. Gated by `--skip-post-create` (returns `None`).
    async fn reprobe_user_env_if_enabled(
        &self,
        container_id: &str,
        remote_user: &str,
        shell: &str,
        prior: Option<&std::collections::HashMap<String, String>>,
    ) -> Option<std::collections::HashMap<String, String>> {
        if !self.config.lifecycle_gate.enabled {
            return None;
        }
        run_step_result(&self.progress, "Updating environment cache...", async {
            Ok::<_, std::convert::Infallible>(
                crate::env_cache::probe_and_cache_user_env(
                    self.client,
                    container_id,
                    remote_user,
                    self.config.user_env_probe,
                    shell,
                )
                .await
                .or_else(|| prior.cloned()),
            )
        })
        .await
        .ok()
        .flatten()
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

        // --skip-post-create gates the userEnvProbe (official: computeRemoteEnv
        // === lifecycleHook.enabled, so probeRemoteEnv is suppressed). Env
        // injection, agent registration, and tool install still run.
        let probed_env = self
            .probe_user_env_if_enabled(container_id, remote_user, &shell)
            .await;

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

        let tools_to_install = crate::tool_install::resolve_tool_names(&settings.tools.install);
        let spec = crate::tool_install::InstallSpec {
            settings: &settings,
            tools: &tools_to_install,
            probed_env: probed_env.as_ref(),
        };
        crate::tool_install::install_tools(
            self.client,
            container_id,
            remote_user,
            &shell,
            &spec,
            &self.progress,
        )
        .await;

        let final_probed = if tools_to_install.is_empty() {
            probed_env
        } else {
            self.reprobe_user_env_if_enabled(container_id, remote_user, &shell, probed_env.as_ref())
                .await
                .or(probed_env)
        };

        let combined_remote_env = self.lifecycle_remote_env(self.config.remote_env);
        let mut lifecycle_env = final_probed.as_ref().map_or_else(
            || combined_remote_env.clone(),
            |probed| cella_env::user_env_probe::merge_env(probed, &combined_remote_env),
        );
        if !self.config.lifecycle_secrets.is_empty() {
            lifecycle_env.extend_from_slice(self.config.lifecycle_secrets);
        }

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
        // --skip-post-create gates every lifecycle hook, including the deferred
        // onCreateCommand for prebuilt images.
        if !self.config.lifecycle_gate.enabled {
            return Ok(());
        }
        let lc_state = read_lifecycle_state(self.client, container_id, remote_user).await;
        if lc_state.oncreate_done {
            return Ok(());
        }
        let lc_ctx = self.build_lifecycle_ctx(container_id, remote_user, lifecycle_env);
        let mut entries =
            lifecycle_entries_for_phase(Some(metadata), self.config_json(), "onCreateCommand");
        crate::config_map::substitute_lifecycle_entries(
            &mut entries,
            &crate::subst_ctx(self.config.resolved),
        );
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

        let gate = self.config.lifecycle_gate;
        let lc_ctx_content = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        let subst = crate::subst_ctx(self.config.resolved);
        let subst_clone = subst.clone();
        check_and_run_content_update(
            &lc_ctx_content,
            self.config_json(),
            metadata.map(String::as_str),
            &self.config.resolved.workspace_root,
            &self.progress,
            gate,
            Some(&move |entries| {
                crate::config_map::substitute_lifecycle_entries(entries, &subst_clone);
            }),
        )
        .await?;

        // postAttachCommand runs on every attach (no marker). Gate it for
        // --skip-post-create (entire chain), --skip-post-attach (this phase),
        // and the stop-after flags (--prebuild / --skip-non-blocking-commands
        // stop before postAttach). plan_phases is the single source of truth.
        if gate.runs_post_attach() {
            let mut entries = lifecycle_entries_for_phase(
                metadata.map(String::as_str),
                self.config_json(),
                "postAttachCommand",
            );
            crate::config_map::substitute_lifecycle_entries(&mut entries, &subst);
            let lc_ctx = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
            run_lifecycle_entries(&lc_ctx, "postAttachCommand", &entries, &self.progress).await?;
        }

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

        let gate = self.config.lifecycle_gate;
        let lc_ctx = self.build_lifecycle_ctx(&container.id, remote_user, &lifecycle_env);
        let subst = crate::subst_ctx(self.config.resolved);
        let subst_clone = subst.clone();
        check_and_run_content_update(
            &lc_ctx,
            self.config_json(),
            metadata.map(String::as_str),
            &self.config.resolved.workspace_root,
            &self.progress,
            gate,
            Some(&move |entries| {
                crate::config_map::substitute_lifecycle_entries(entries, &subst_clone);
            }),
        )
        .await?;
        // Honor the gate: --skip-post-create drops both; the stop-after flags
        // (--prebuild / --skip-non-blocking-commands) stop before postStart;
        // --skip-post-attach drops only postAttachCommand.
        for phase in ["postStartCommand", "postAttachCommand"] {
            let phase_enabled = match phase {
                "postStartCommand" => gate.runs_post_start(),
                _ => gate.runs_post_attach(),
            };
            if !phase_enabled {
                continue;
            }
            let mut entries = lifecycle_entries_for_phase(
                metadata.map(String::as_str),
                self.config_json(),
                phase,
            );
            crate::config_map::substitute_lifecycle_entries(&mut entries, &subst);
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

    async fn setup_credential_protection(
        &self,
        container_id: &str,
        settings: &cella_config::CellaConfig,
        remote_user: &str,
    ) {
        let phantom_set = crate::credential_protect::generate_phantom_tokens(settings);

        if phantom_set.entries.is_empty() {
            tracing::debug!("Credential protection enabled but no providers detected");
            return;
        }

        let Some(socket_path) = cella_env::paths::daemon_socket_path() else {
            tracing::warn!("Cannot determine daemon socket path, skipping credential protection");
            return;
        };

        let container_name = self.config.container_name.to_string();
        let nonce = crate::credential_protect::register_with_daemon(
            &socket_path,
            &container_name,
            &phantom_set.entries,
        )
        .await;

        if nonce.is_none() {
            tracing::warn!(
                "Phantom token registration failed — credentials will be unavailable \
                 (credential protection does not fall back to real credentials)"
            );
            return;
        }

        if settings.credentials.gh {
            if let Some(ref gh_phantom) = phantom_set.gh_phantom {
                if let Some(gh_creds) = cella_env::gh_credential::prepare_gh_credentials_phantom(
                    &self.config.resolved.workspace_root,
                    remote_user,
                    gh_phantom,
                ) {
                    let config_dir = cella_env::gh_credential::gh_config_dir_for_user(remote_user);
                    let files: Vec<FileToUpload> = gh_creds
                        .file_uploads
                        .iter()
                        .map(|f| FileToUpload {
                            path: f.container_path.clone(),
                            content: f.content.clone(),
                            mode: f.mode,
                        })
                        .collect();
                    let _ = self
                        .client
                        .exec_command(
                            container_id,
                            &ExecOptions {
                                cmd: vec![
                                    "mkdir".to_string(),
                                    "-p".to_string(),
                                    "-m".to_string(),
                                    "700".to_string(),
                                    config_dir.clone(),
                                ],
                                user: Some("root".to_string()),
                                env: None,
                                working_dir: None,
                            },
                        )
                        .await;
                    let _ = self.client.upload_files(container_id, &files).await;
                    tracing::debug!("Seeded phantom gh credentials into container");
                }
            } else {
                crate::container_setup::seed_gh_credentials(
                    self.client,
                    container_id,
                    &self.config.resolved.workspace_root,
                    remote_user,
                )
                .await;
            }
        }

        tracing::info!(
            "Credential protection active: {} providers protected",
            phantom_set.entries.len()
        );
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
        labels.insert(
            "dev.cella.user_env_probe".to_string(),
            self.config.user_env_probe.to_string(),
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
                cella_features::generate_metadata_label(
                    &[],
                    config,
                    None,
                    self.config.metadata_options.omit_remote_env,
                ),
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
        // CLI --id-label values are set on the container so a later `up`
        // --id-label finds it (AND-matched), additive on top of cella labels.
        for id_label in self.config.id_labels {
            if let Some((k, v)) = id_label.split_once('=') {
                labels.insert(k.to_string(), v.to_string());
            }
        }
        if runtime == cella_env::DockerRuntime::OrbStack {
            add_orbstack_hostname_labels(
                &mut labels,
                config,
                &self.config.resolved.workspace_root,
                self.config.extra_labels,
            );
        }
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
                external: false,
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

        // Opt the container into ~/.claude.json sync (env is immutable after
        // create, so it must be set here at create time).
        let tool_env = crate::tool_install::tool_config_env_vars(settings, remote_user);
        if !tool_env.is_empty() {
            if create_opts.env.is_empty() {
                create_opts.env = image_env.to_vec();
            }
            create_opts.env.extend(tool_env);
        }

        append_extra_mounts(
            &mut create_opts.mounts,
            &self.config.resolved.workspace_root,
            remote_user,
            self.config.mount_flags.additional_cli_mounts,
            self.client,
            capabilities.managed_agent,
        );

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

        if settings.credentials.protect {
            self.setup_credential_protection(container_id, settings, remote_user)
                .await;
        } else {
            if settings.credentials.gh {
                crate::container_setup::seed_gh_credentials(
                    self.client,
                    container_id,
                    &self.config.resolved.workspace_root,
                    remote_user,
                )
                .await;
            }

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
        }

        let shell = crate::shell_detect::detect_shell(self.client, container_id, remote_user).await;
        let probed_env = self
            .probe_user_env_if_enabled(container_id, remote_user, &shell)
            .await;

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

        // Seed single-file configs (~/.claude.json, ~/.tmux.conf) as regular
        // files instead of single-file bind mounts (anti-ghost). No-ops when
        // nothing is forwarded or the files already exist in the container.
        crate::tool_install::seed_tool_config_files(
            self.client,
            container_id,
            settings,
            remote_user,
        )
        .await;

        let tools_to_install = crate::tool_install::resolve_tool_names(&settings.tools.install);
        let spec = crate::tool_install::InstallSpec {
            settings,
            tools: &tools_to_install,
            probed_env: probed_env.as_ref(),
        };
        crate::tool_install::install_tools(
            self.client,
            container_id,
            remote_user,
            shell,
            &spec,
            &self.progress,
        )
        .await;

        let final_probed = if tools_to_install.is_empty() {
            probed_env
        } else {
            self.reprobe_user_env_if_enabled(container_id, remote_user, shell, probed_env.as_ref())
                .await
                .or(probed_env)
        };

        let mut lifecycle_env = final_probed.as_ref().map_or_else(
            || remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, remote_env),
        );
        if !self.config.lifecycle_secrets.is_empty() {
            lifecycle_env.extend_from_slice(self.config.lifecycle_secrets);
        }

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
        let needs_proxy = (has_rules || settings.credentials.protect) && managed_agent;
        let proxy_fwd = Some(cella_env::ProxyForwardingConfig {
            proxy: net_config.proxy.clone(),
            has_blocking_rules: needs_proxy,
            full_config: if needs_proxy { Some(net_config) } else { None },
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

        if settings.credentials.protect && managed_agent {
            crate::credential_protect::inject_routes_into_proxy_config(
                &settings,
                self.config.container_name,
                &mut env_fwd,
            );
        }

        let mut labels = self.build_labels(
            resolved_features,
            base_image_details.metadata.as_deref(),
            &env_fwd,
            &remote_user,
        );

        if settings.credentials.protect {
            crate::credential_protect::add_protect_label(&mut labels, self.config.container_name);
        }

        let subst_ctx = crate::subst_ctx(self.config.resolved);
        let substituted_feature_config = resolved_features.map(|r| {
            crate::config_map::substitute_feature_config(r.container_config.clone(), &subst_ctx)
        });
        let image_meta_config = if substituted_feature_config.is_none() {
            base_image_details.metadata.as_deref().map(|m| {
                let cfg = cella_features::parse_image_metadata(m).0;
                crate::config_map::substitute_feature_config(cfg, &subst_ctx)
            })
        } else {
            None
        };
        let effective_feature_config = substituted_feature_config
            .as_ref()
            .or(image_meta_config.as_ref());

        let host_mount_folder = cella_git::find_git_root_folder(
            &self.config.resolved.workspace_root,
            self.config.mount_flags.mount_workspace_git_root,
        );

        let worktree_result = resolve_worktree_common_dir(
            &host_mount_folder,
            self.config.mount_flags.mount_workspace_git_root,
            self.config.mount_flags.mount_git_worktree_common_dir,
            self.config.mount_flags.workspace_mount_consistency,
        );

        let mut create_opts = crate::config_map::map_config(crate::config_map::MapConfigParams {
            config,
            container_name: self.config.container_name,
            image_name: img_name,
            labels,
            workspace_root: &self.config.resolved.workspace_root,
            host_mount_folder: &host_mount_folder,
            feature_config: effective_feature_config,
            image_env: &image_env,
            agent_arch,
            workspace_mount_consistency: self.config.mount_flags.workspace_mount_consistency,
        });

        if let Some(wt) = worktree_result {
            create_opts.mounts.push(wt.mount);
        }

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
        let gate = self.config.lifecycle_gate;
        let lc_ctx = self.build_lifecycle_ctx(container_id, remote_user, lifecycle_env);
        let subst = crate::subst_ctx(self.config.resolved);
        run_lifecycle_phases_with_wait_for(
            &lc_ctx,
            config,
            resolved_features,
            image_metadata,
            &self.progress,
            gate,
            Some(&move |entries| {
                crate::config_map::substitute_lifecycle_entries(entries, &subst);
            }),
        )
        .await?;

        // --skip-post-create: nothing ran. Do NOT write the content hash —
        // doing so would make a later un-gated `up` see a matching hash and
        // permanently skip the deferred updateContent + postCreate. Force
        // oncreate_done=false so onCreate also runs on the next invocation.
        if !gate.enabled {
            let state = LifecycleState {
                oncreate_done: false,
            };
            write_lifecycle_state(self.client, container_id, remote_user, &state).await;
            return Ok(());
        }

        // The content hash gates updateContent + postCreate together on reuse.
        // Persist it only when postCreateCommand actually ran here — otherwise
        // a later un-gated `up` would see a matching hash and skip the deferred
        // postCreate. This mirrors check_and_run_content_update's reuse gating
        // (both keyed on whether postCreate ran), so create and reuse agree.
        // Backgrounded postCreate (standard `up`) is written by the background
        // script, so we still write here for the foreground-or-background case.
        if gate.runs_phase("postCreateCommand") {
            write_content_hash(
                self.client,
                container_id,
                remote_user,
                &self.config.resolved.workspace_root,
            )
            .await;
        }

        // Dotfiles install runs in the FOREGROUND right after the lifecycle
        // phases, mirroring official's slot between postCreate and postStart
        // (injectHeadless.ts:392). Gated identically to write_content_hash so
        // --skip-post-create / --prebuild / --skip-non-blocking-commands all
        // suppress it. Non-fatal: a failure warns but never fails `up`.
        self.install_dotfiles_if_enabled(container_id, remote_user, lifecycle_env, gate)
            .await;

        // Mark onCreateCommand done only when it ran in the FOREGROUND. The
        // phases array is [onCreate, updateContent, postCreate, postStart,
        // postAttach]; onCreate (index 0) runs foreground iff the boundary is
        // past it. When the boundary is 0 (waitFor=initializeCommand with no
        // earlier stop) ALL phases are backgrounded and the background script
        // writes the state file on completion, so we must NOT claim done here.
        let oncreate_foreground = gate.foreground_boundary() > 0;
        let state = LifecycleState {
            oncreate_done: oncreate_foreground,
        };
        write_lifecycle_state(self.client, container_id, remote_user, &state).await;

        Ok(())
    }

    /// Install dotfiles after the create lifecycle, if armed and gate-permitted.
    ///
    /// Runs only when a `--dotfiles-repository` was given AND the gate would run
    /// `postCreateCommand` (so it's skipped under `--skip-post-create`,
    /// `--prebuild`, and `--skip-non-blocking-commands` with the default
    /// `waitFor`). A dotfiles failure is logged as a warning and a progress
    /// message — it never fails `up`, matching the official tool.
    async fn install_dotfiles_if_enabled(
        &self,
        container_id: &str,
        remote_user: &str,
        lifecycle_env: &[String],
        gate: cella_backend::LifecycleGate,
    ) {
        let cfg = &self.config.dotfiles;
        if !should_run_dotfiles(gate, cfg.repository.as_deref()) {
            return;
        }
        let Some(repository) = cfg.repository.as_deref() else {
            return;
        };
        let step = self.progress.step("Installing dotfiles...");
        match crate::dotfiles::install_dotfiles(
            self.client,
            container_id,
            remote_user,
            repository,
            cfg.install_command.as_deref(),
            &cfg.target_path,
            lifecycle_env,
        )
        .await
        {
            Ok(()) => step.finish(),
            Err(e) => {
                warn!("Dotfiles install failed (continuing): {e}");
                step.fail("failed");
                self.progress
                    .warn(&format!("Dotfiles install failed (continuing): {e}"));
            }
        }
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
    /// Non-SSH bind mount failures (e.g. worktree paths not yet visible)
    /// are retried up to 3 times with a 500ms delay.
    async fn create_container_with_ssh_fallback(
        &self,
        create_opts: &mut cella_backend::CreateContainerOptions,
        env_fwd: &mut cella_env::EnvForwarding,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        const MAX_BIND_MOUNT_RETRIES: u32 = 3;
        let mut bind_mount_attempts: u32 = 0;

        loop {
            match self.create_container(create_opts).await {
                Ok(id) => return Ok(id),
                Err(e) => {
                    let err_msg = e.to_string();

                    if cella_env::ssh_agent::is_ssh_mount_error(
                        &err_msg,
                        env_fwd.ssh_agent_mount_source.as_deref(),
                    ) {
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
                                        external: false,
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

                    if cella_env::ssh_agent::is_bind_mount_error(&err_msg, None)
                        && bind_mount_attempts < MAX_BIND_MOUNT_RETRIES
                    {
                        bind_mount_attempts += 1;
                        warn!(
                            attempt = bind_mount_attempts,
                            max = MAX_BIND_MOUNT_RETRIES,
                            "Bind mount source path not found, retrying in 500ms"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }

                    return Err(e);
                }
            }
        }
    }

    /// Resolve whether a config-requested GPU should be granted.
    ///
    /// Mirrors official's `checkDockerSupportForGPU`: `All` always grants,
    /// `None` never grants, `Detect` probes the daemon for an NVIDIA runtime.
    async fn gpu_support_available(&self) -> bool {
        match self.config.gpu_availability {
            cella_backend::GpuAvailability::All => true,
            cella_backend::GpuAvailability::None => false,
            cella_backend::GpuAvailability::Detect => {
                self.client.detect_gpu_support().await.unwrap_or(false)
            }
        }
    }

    /// Apply `--gpu-availability` to a config-derived GPU request.
    ///
    /// `create_opts.gpu_request` carries the `hostRequirements.gpu` request
    /// (runArgs `--gpus` lives separately and is never touched here). When the
    /// resolved policy declines GPU support, the request is stripped; a missing,
    /// non-`optional` requirement then emits the official warning. When granted,
    /// the request is normalized to `All` (official always emits `--gpus all` /
    /// capabilities `[["gpu"]]` — a `cores` count is never honored).
    async fn gate_gpu_request(
        &self,
        config: &serde_json::Value,
        create_opts: &mut cella_backend::CreateContainerOptions,
    ) {
        if create_opts.gpu_request.is_none() {
            return;
        }

        if self.gpu_support_available().await {
            create_opts.gpu_request = Some(cella_backend::GpuRequest::All);
        } else {
            create_opts.gpu_request = None;
            let is_optional = config
                .get("hostRequirements")
                .and_then(|h| h.get("gpu"))
                .and_then(serde_json::Value::as_str)
                == Some("optional");
            if !is_optional {
                self.progress.warn(
                    "No GPU support found yet a GPU was required - consider marking it as \"optional\"",
                );
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
        let config_value = config
            .get("updateRemoteUserUID")
            .and_then(serde_json::Value::as_bool);
        let update_uid = cella_backend::should_update_uid(
            config_value,
            self.config.update_remote_user_uid_default,
        );

        if update_uid
            && let Some(uid_img) = crate::uid_image::build_uid_remap_image(
                self.client,
                img_name,
                image_user,
                remote_user,
                cella_backend::uid_image::BuildToolchain {
                    docker_path: self.config.build_tuning.docker_path,
                    use_buildkit: self.config.build_tuning.use_buildkit,
                },
                &self.progress,
            )
            .await?
        {
            create_opts.image = uid_img;
        }
        Ok(())
    }

    /// Lifecycle command environment: CLI `--remote-env` first, then `base`
    /// (config `remoteEnv`) so config wins on collision (merge is later-wins).
    /// Lifecycle-only — never enters labels or `containerEnv`.
    fn lifecycle_remote_env(&self, base: &[String]) -> Vec<String> {
        self.config
            .cli_remote_env
            .iter()
            .chain(base.iter())
            .cloned()
            .collect()
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
                build_tuning: self.config.build_tuning,
                omit_remote_env_from_metadata: self.config.metadata_options.omit_remote_env,
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

        self.gate_gpu_request(config, &mut create_opts).await;

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
        crate::tool_install::ensure_tool_config_paths(&settings);
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

        let create_lifecycle_remote_env = self.lifecycle_remote_env(&create_opts.remote_env);
        let (_probed_env, lifecycle_env) = self
            .post_create_setup(
                &container_id,
                &remote_user,
                &env_fwd,
                &settings,
                &create_lifecycle_remote_env,
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

        // With --id-label, find by AND-matched labels (matching the official
        // CLI); otherwise fall back to the workspace-path lookup.
        let existing = if self.config.id_labels.is_empty() {
            self.client
                .find_container(&self.config.resolved.workspace_root)
                .await?
        } else {
            self.client
                .find_container_by_labels(self.config.id_labels)
                .await?
        };

        // --expect-existing-container: fail before any build/create if no
        // container exists. Must precede the remove/create logic (matches
        // official ordering: the expect check runs before removeOnStartup).
        // A stopped container counts as existing (find returns it), so this
        // only fires when nothing was found.
        if existing.is_none() && self.config.expect_existing_container {
            return Err(EXPECTED_CONTAINER_MISSING.into());
        }

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

fn add_orbstack_hostname_labels(
    labels: &mut std::collections::HashMap<String, String>,
    config: &serde_json::Value,
    workspace_root: &std::path::Path,
    extra_labels: &std::collections::HashMap<String, String>,
) {
    let forward_ports = parse_forward_ports(config);
    let Some(default_port) = cella_backend::orbstack_http_port_label(&forward_ports) else {
        return;
    };
    let project = config
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|name| !name.trim().is_empty())
        .map_or_else(
            || {
                workspace_root.file_name().map_or_else(
                    || "workspace".to_string(),
                    |n| n.to_string_lossy().to_string(),
                )
            },
            String::from,
        );
    let branch = extra_labels
        .get("dev.cella.branch")
        .cloned()
        .or_else(|| {
            cella_git::discover(workspace_root)
                .ok()
                .and_then(|r| r.head_branch)
        })
        .unwrap_or_else(|| "main".to_string());

    labels.insert(
        "dev.orbstack.domains".to_string(),
        cella_backend::orbstack_domains_label(&project, &branch),
    );
    labels.insert("dev.orbstack.http-port".to_string(), default_port);
}

/// Whether dotfiles should be installed for this `up`.
///
/// Dotfiles run iff a repository was given AND the gate would run
/// `postStartCommand` — in the official tool they occupy the slot AFTER the
/// `postCreate` `skipNonBlocking` checkpoint (`injectHeadless.ts:392`, past the
/// `return` at :388), so any flag that stops the lifecycle at or before
/// `postCreate` also skips dotfiles. Keying on `postStartCommand` (not
/// `postCreateCommand`) is what makes `--skip-non-blocking-commands` with
/// `waitFor: postCreateCommand` correctly skip dotfiles, matching the official
/// behavior across every `waitFor` value.
fn should_run_dotfiles(gate: cella_backend::LifecycleGate, repository: Option<&str>) -> bool {
    repository.is_some() && gate.enabled && gate.runs_phase("postStartCommand")
}

fn parse_forward_ports(config: &serde_json::Value) -> Vec<u16> {
    config
        .get("forwardPorts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default()
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

struct WorktreeCommonDirResult {
    mount: MountConfig,
}

fn resolve_worktree_common_dir(
    host_mount_folder: &std::path::Path,
    mount_workspace_git_root: bool,
    mount_git_worktree_common_dir: bool,
    consistency: Option<&str>,
) -> Option<WorktreeCommonDirResult> {
    if !(mount_workspace_git_root && mount_git_worktree_common_dir) {
        return None;
    }

    let dot_git = host_mount_folder.join(".git");
    if !dot_git.is_file() {
        return None;
    }

    let content = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = content.strip_prefix("gitdir: ")?.trim();

    let gitdir_path = std::path::Path::new(gitdir);
    if gitdir_path.is_absolute() {
        return None;
    }

    // Compute the git common dir (two levels up from gitdir).
    // e.g. gitdir: ../../.git/worktrees/my-wt → common dir = ../../.git → resolved
    let resolved_gitdir = host_mount_folder.join(gitdir);
    let git_common_dir = resolved_gitdir.parent()?.parent()?.to_path_buf();
    let git_common_dir = git_common_dir.canonicalize().unwrap_or(git_common_dir);

    // Collect path segments from host_mount_folder up to where git_common_dir is reachable
    let mut segments = Vec::new();
    let mut current = host_mount_folder.to_path_buf();
    loop {
        if git_common_dir.starts_with(&current) {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        if let Some(name) = current.file_name() {
            segments.insert(0, name.to_string_lossy().to_string());
        }
        current = parent.to_path_buf();
    }

    let container_mount_folder = format!("/workspaces/{}", segments.join("/"));

    // Compute container git common dir by resolving the relative gitdir from
    // the container mount folder
    let container_gitdir = std::path::PathBuf::from(&container_mount_folder).join(gitdir);
    let container_git_common = container_gitdir.parent()?.parent()?.to_path_buf();
    // Normalize the path (remove . and ..)
    let container_git_common_str = normalize_path_string(&container_git_common.to_string_lossy());

    let cons = if cfg!(target_os = "linux") {
        None
    } else {
        Some(consistency.unwrap_or("consistent").to_string())
    };

    Some(WorktreeCommonDirResult {
        mount: MountConfig {
            mount_type: "bind".to_string(),
            source: git_common_dir.to_string_lossy().to_string(),
            target: container_git_common_str,
            consistency: cons,
            read_only: false,
            external: false,
        },
    })
}

/// Append worktree parent git dir, agent IPC dirs, CLI mounts, and agent volume.
fn append_extra_mounts(
    mounts: &mut Vec<MountConfig>,
    workspace_root: &std::path::Path,
    remote_user: &str,
    additional_cli_mounts: &[MountConfig],
    client: &dyn ContainerBackend,
    managed_agent: bool,
) {
    if let Some(parent_git) = cella_git::parent_git_dir(workspace_root) {
        let canonical = parent_git
            .canonicalize()
            .unwrap_or_else(|_| parent_git.clone());
        let path_str = canonical.to_string_lossy().to_string();
        mounts.push(MountConfig {
            mount_type: "bind".to_string(),
            source: path_str.clone(),
            target: path_str,
            consistency: None,
            read_only: false,
            external: false,
        });
        append_agent_ipc_mounts(mounts, remote_user);
    }

    for m in additional_cli_mounts {
        mounts.push(m.clone());
    }

    let (vol_name, vol_target, _ro) = client.agent_volume_mount();
    if managed_agent && !vol_name.is_empty() {
        mounts.push(MountConfig {
            mount_type: "volume".to_string(),
            source: vol_name,
            target: vol_target,
            consistency: None,
            read_only: false,
            external: false,
        });
    }
}

/// Bind-mount agent IPC directories (Claude Code teams/tasks, Codex queues)
/// from the host into the container for cross-container communication.
fn append_agent_ipc_mounts(mounts: &mut Vec<MountConfig>, remote_user: &str) {
    let Ok(home_str) = std::env::var("HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(&home_str);
    let container_home = if remote_user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{remote_user}")
    };
    let ipc_dirs = [
        (".claude/teams", ".claude/teams"),
        (".claude/tasks", ".claude/tasks"),
        (".acpx", ".acpx"),
    ];
    for (host_rel, target_rel) in ipc_dirs {
        let host_path = home.join(host_rel);
        if host_path.is_dir() {
            mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target: format!("{container_home}/{target_rel}"),
                consistency: None,
                read_only: false,
                external: false,
            });
        }
    }
}

fn normalize_path_string(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(component),
        }
    }
    let normalized = parts.join("/");
    if path.starts_with('/') {
        format!("/{normalized}")
    } else {
        normalized
    }
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

    use cella_backend::{LifecycleGate, StopAfter, WaitForPhase};

    #[test]
    fn network_rule_policy_enforce_eq() {
        assert_eq!(NetworkRulePolicy::Enforce, NetworkRulePolicy::Enforce);
        assert_ne!(NetworkRulePolicy::Enforce, NetworkRulePolicy::Skip);
    }

    #[test]
    fn dotfiles_skipped_when_repository_is_none() {
        // No repository: nothing to install, even on a fully-enabled gate.
        assert!(!should_run_dotfiles(LifecycleGate::default(), None));
    }

    #[test]
    fn dotfiles_skipped_when_gate_disabled() {
        // --skip-post-create disables the whole hook chain incl. dotfiles.
        let gate = LifecycleGate {
            enabled: false,
            ..LifecycleGate::default()
        };
        assert!(!should_run_dotfiles(gate, Some("owner/repo")));
    }

    #[test]
    fn dotfiles_skipped_under_prebuild() {
        // --prebuild stops after updateContent, before postCreate -> skip.
        let gate = LifecycleGate {
            stop: StopAfter {
                prebuild: true,
                skip_non_blocking: false,
            },
            ..LifecycleGate::default()
        };
        assert!(!should_run_dotfiles(gate, Some("owner/repo")));
    }

    #[test]
    fn dotfiles_skipped_under_skip_non_blocking_default_wait_for() {
        // --skip-non-blocking-commands at default waitFor=updateContent stops
        // before postCreate -> skip dotfiles.
        let gate = LifecycleGate {
            wait_for: WaitForPhase::UpdateContent,
            stop: StopAfter {
                prebuild: false,
                skip_non_blocking: true,
            },
            ..LifecycleGate::default()
        };
        assert!(!should_run_dotfiles(gate, Some("owner/repo")));
    }

    #[test]
    fn dotfiles_skipped_under_skip_non_blocking_wait_for_post_create() {
        // Regression: --skip-non-blocking-commands with waitFor=postCreateCommand
        // stops right AFTER postCreate (before the dotfiles slot), so dotfiles
        // must NOT install — matching official injectHeadless.ts (return at :388
        // before the dotfiles call at :392). Gating on postCreate would wrongly
        // install here; gating on postStart correctly skips.
        let gate = LifecycleGate {
            wait_for: WaitForPhase::PostCreate,
            stop: StopAfter {
                prebuild: false,
                skip_non_blocking: true,
            },
            ..LifecycleGate::default()
        };
        assert!(!should_run_dotfiles(gate, Some("owner/repo")));
    }

    #[test]
    fn dotfiles_runs_under_skip_non_blocking_wait_for_post_start() {
        // --skip-non-blocking-commands with waitFor=postStart runs through
        // postCreate, so dotfiles install.
        let gate = LifecycleGate {
            wait_for: WaitForPhase::PostStart,
            stop: StopAfter {
                prebuild: false,
                skip_non_blocking: true,
            },
            ..LifecycleGate::default()
        };
        assert!(should_run_dotfiles(gate, Some("owner/repo")));
    }

    #[test]
    fn dotfiles_runs_on_default_gate_with_repository() {
        // Happy path: standard `up` with a repository installs dotfiles.
        assert!(should_run_dotfiles(
            LifecycleGate::default(),
            Some("owner/repo")
        ));
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

    #[test]
    fn orbstack_hostname_labels_use_default_port_only() {
        let mut labels = std::collections::HashMap::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert("dev.cella.branch".to_string(), "feature/auth".to_string());
        let config = serde_json::json!({
            "name": "myapp",
            "forwardPorts": [3000, 8080]
        });

        add_orbstack_hostname_labels(
            &mut labels,
            &config,
            std::path::Path::new("/tmp/app"),
            &extra,
        );

        assert_eq!(
            labels.get("dev.orbstack.domains").map(String::as_str),
            Some("feature-auth.myapp.local")
        );
        assert_eq!(
            labels.get("dev.orbstack.http-port").map(String::as_str),
            Some("3000")
        );
        assert!(
            !labels
                .get("dev.orbstack.domains")
                .unwrap()
                .contains("8080.")
        );
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

    // ── normalize_path_string ─────────────────────────────────────

    #[test]
    fn normalize_removes_dot_segments() {
        assert_eq!(normalize_path_string("/a/b/../c"), "/a/c");
    }

    #[test]
    fn normalize_preserves_absolute_prefix() {
        assert_eq!(normalize_path_string("/a/b/c"), "/a/b/c");
    }

    #[test]
    fn normalize_collapses_multiple_dot_dot() {
        assert_eq!(normalize_path_string("/a/b/c/../../d"), "/a/d");
    }

    #[test]
    fn normalize_handles_relative_path() {
        assert_eq!(normalize_path_string("a/b/../c"), "a/c");
    }

    // ── resolve_worktree_common_dir ───────────────────────────────

    #[test]
    fn worktree_common_dir_not_a_worktree() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let result = resolve_worktree_common_dir(tmp.path(), true, true, None);
        assert!(result.is_none(), ".git directory → not a worktree");
    }

    #[test]
    fn worktree_common_dir_disabled() {
        let result = resolve_worktree_common_dir(std::path::Path::new("/tmp"), true, false, None);
        assert!(result.is_none(), "must return None when flag is disabled");
    }

    #[test]
    fn worktree_common_dir_git_root_disabled() {
        let result = resolve_worktree_common_dir(std::path::Path::new("/tmp"), false, true, None);
        assert!(
            result.is_none(),
            "must return None when git root mount is disabled"
        );
    }

    #[test]
    fn worktree_common_dir_absolute_gitdir_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".git"),
            "gitdir: /absolute/path/.git/worktrees/wt",
        )
        .unwrap();
        let result = resolve_worktree_common_dir(tmp.path(), true, true, None);
        assert!(result.is_none(), "absolute gitdir paths must be ignored");
    }

    #[test]
    fn worktree_common_dir_relative_gitdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("worktrees/my-wt")).unwrap();
        let worktree_dir = tmp.path().join("worktrees").join("my-wt");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(
            worktree_dir.join(".git"),
            "gitdir: ../../.git/worktrees/my-wt",
        )
        .unwrap();

        let result = resolve_worktree_common_dir(&worktree_dir, true, true, None);
        assert!(result.is_some(), "should resolve relative gitdir");
        let wt = result.unwrap();
        assert_eq!(wt.mount.mount_type, "bind");
    }
}
