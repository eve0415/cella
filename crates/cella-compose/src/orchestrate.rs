//! Docker Compose orchestration for `cella up` when `dockerComposeFile` is present.
//!
//! This module contains the core compose pipeline logic. CLI-specific operations
//! (daemon management, agent launch, output formatting) are injected via the
//! [`ComposeUpHooks`] trait.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tracing::{debug, info, warn};

use cella_backend::agent::restart_agent_in_container;
use cella_backend::container_setup::{
    resolve_remote_user, run_host_command, verify_container_running,
};
use cella_backend::lifecycle::{lifecycle_entries_for_phase, run_lifecycle_entries};
use cella_backend::progress::ProgressSender;
use cella_backend::{
    ContainerBackend, ContainerInfo, ContainerState, LifecycleContext, MountSpec,
    SshAgentProxyStatus, agent_env_vars, names::lexical_absolute, run_lifecycle_phase,
};
use cella_config::devcontainer::resolve::ResolvedConfig;

use crate::{ComposeCommand, ComposeProject, OverrideConfig, ServiceBuildInfo};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a compose up invocation.
pub struct ComposeUpConfig<'a> {
    /// Fully resolved devcontainer configuration.
    pub resolved: &'a ResolvedConfig,
    /// Parsed devcontainer JSON.
    pub config: &'a serde_json::Value,
    /// Path to devcontainer.json.
    pub config_path: &'a Path,
    /// Workspace root on the host.
    pub workspace_root: &'a Path,
    /// Container name for daemon registration.
    pub container_name: &'a str,
    /// Extra environment variables to inject (`KEY=VALUE` format). From config
    /// `remoteEnv`; used for lifecycle env AND the metadata label / containerEnv.
    pub remote_env: &'a [String],
    /// CLI `--remote-env` entries. Lifecycle command env ONLY (config
    /// `remote_env` wins on collision); never enters labels or `containerEnv`.
    pub cli_remote_env: &'a [String],
    /// How to resolve an existing/missing container before building.
    pub resolution: ContainerResolution,
    /// Skip agent checksum verification.
    pub skip_checksum: bool,
    /// Docker Compose profiles to activate (`--profile` flags).
    pub profiles: Vec<String>,
    /// Extra env-file paths for docker compose (`--env-file` flags).
    pub env_files: Vec<PathBuf>,
    /// Pull policy for docker compose up/build (`--pull` flag).
    pub pull_policy: Option<String>,
    /// Network rule enforcement policy.
    pub network_rule_policy: cella_network::NetworkRulePolicy,
    /// Resolved userEnvProbe type (from config or CLI default).
    pub user_env_probe: cella_env::user_env_probe::UserEnvProbe,
    /// Gates which lifecycle phases run for this compose `up` (built from
    /// `--skip-post-create` / `--skip-non-blocking-commands` / `--prebuild` /
    /// `--skip-post-attach`). `Default` runs every phase.
    pub lifecycle_gate: cella_backend::LifecycleGate,
    /// Build/backend tuning (`--docker-path`, `--docker-compose-path`,
    /// `--buildkit`).
    pub build_tuning: ComposeBuildTuning,
    /// `--gpu-availability`: whether a config-requested GPU is granted.
    pub gpu_availability: cella_backend::GpuAvailability,
    /// `--update-remote-user-uid-default`: default for `updateRemoteUserUID`.
    pub update_remote_user_uid_default: cella_backend::UpdateRemoteUserUidDefault,
    /// `--omit-config-remote-env-from-metadata`: strip `remoteEnv` from the
    /// generated `devcontainer.metadata` label. Does NOT affect the runtime
    /// `dev.cella.remote_env` label.
    pub omit_remote_env_from_metadata: bool,
    /// Feature lockfile policy derived from `--no-lockfile` / `--frozen-lockfile`.
    /// Threaded into compose feature resolution so dockerCompose devcontainers
    /// write/validate `devcontainer-lock.json` like single-container builds.
    pub lockfile_policy: cella_features::LockfilePolicy,
    /// Dotfiles install inputs (`--dotfiles-repository` / `-install-command` /
    /// `-target-path`). Installed via [`ComposeUpHooks::install_dotfiles`] in
    /// the post-create flow, between `postCreateCommand` and `postStartCommand`,
    /// when `repository` is `Some` and the gate runs `postCreateCommand`.
    pub dotfiles: DotfilesConfig,
}

impl ComposeUpConfig<'_> {
    /// Lifecycle command environment: CLI `--remote-env` first, config
    /// `remoteEnv` last so config wins on collision (the merge is later-wins).
    /// Lifecycle-only — never enters labels or `containerEnv`.
    fn lifecycle_remote_env(&self) -> Vec<String> {
        self.cli_remote_env
            .iter()
            .chain(self.remote_env.iter())
            .cloned()
            .collect()
    }
}

/// Dotfiles installation inputs resolved from the `--dotfiles-*` CLI flags.
///
/// Mirrors `cella_orchestrator::config::DotfilesConfig` — duplicated here
/// because cella-orchestrator depends on cella-compose (so cella-compose cannot
/// import the orchestrator's type). `repository` being `Some` arms the install;
/// the value is expected to be already normalized (owner/repo shorthand
/// expanded) by the CLI before it reaches here. The actual clone+install runs
/// via [`ComposeUpHooks::install_dotfiles`] so cella-compose never needs to
/// reach the orchestrator's install logic.
#[derive(Debug, Clone, Default)]
pub struct DotfilesConfig {
    /// `--dotfiles-repository`: clone source. `None` disables dotfiles install.
    pub repository: Option<String>,
    /// `--dotfiles-install-command`: explicit install script. `None` autodetects.
    pub install_command: Option<String>,
    /// `--dotfiles-target-path`: in-container clone target (default `~/dotfiles`).
    pub target_path: String,
}

/// Build/backend tuning inputs for the compose `up` path.
///
/// `docker_path` selects the `docker` binary used for `docker compose`.
/// `docker_compose_path` is the standalone (V1) `docker-compose` binary — cella
/// is V2-only today, so it is accepted and stored but currently unused (see
/// [`ComposeCommand`]). `use_buildkit` propagates the `--buildkit` decision to
/// build sites cella owns (e.g. the UID-remap layer).
#[derive(Debug, Clone, Default)]
pub struct ComposeBuildTuning {
    /// `docker` CLI binary path (`--docker-path`). `None` = `docker`.
    pub docker_path: Option<String>,
    /// Standalone `docker-compose` (V1) binary (`--docker-compose-path`).
    /// Accepted-and-stored; reserved for a future V1 fallback.
    pub docker_compose_path: Option<String>,
    /// Whether `BuildKit`/buildx may be used (`false` = classic builder).
    pub use_buildkit: bool,
}

impl ComposeUpConfig<'_> {
    /// `(docker_path, docker_compose_path)` for [`ComposeCommand::with_docker_binaries`].
    fn docker_binaries(&self) -> (Option<String>, Option<String>) {
        (
            self.build_tuning.docker_path.clone(),
            self.build_tuning.docker_compose_path.clone(),
        )
    }
}

/// How to resolve an existing (or missing) compose container before building.
///
/// Groups the container-resolution flags so they live together and stay under
/// the struct bool-count lint.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContainerResolution {
    /// Whether to tear down and recreate existing containers.
    pub remove_container: bool,
    /// Whether to rebuild with `--no-cache`.
    pub build_no_cache: bool,
    /// `--expect-existing-container`: fail (rather than create) if no compose
    /// container is found. Gates before any build/up.
    pub expect_existing_container: bool,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Result of a compose up operation.
pub struct ComposeUpResult {
    /// Container ID of the primary service.
    pub container_id: String,
    /// Remote user for the container.
    pub remote_user: String,
    /// Workspace folder path inside the container.
    pub workspace_folder: String,
    /// Docker Compose project name (the `-p`/`COMPOSE_PROJECT_NAME` value).
    /// Surfaced as `composeProjectName` in the `up` result envelope.
    pub project_name: String,
    /// Whether the container was freshly created or already running.
    pub outcome: ComposeUpOutcome,
    /// SSH-agent proxy status, when an SSH-agent forwarding decision
    /// was actually surfaced for this container. `None` when the proxy
    /// code path was not exercised.
    pub ssh_agent_proxy: Option<SshAgentProxyStatus>,
}

/// Whether the compose container was created fresh or was already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeUpOutcome {
    /// Container was freshly created via compose up.
    Created,
    /// Container was already running; only postAttachCommand ran.
    Running,
}

// ---------------------------------------------------------------------------
// Hooks for CLI-specific operations
// ---------------------------------------------------------------------------

/// Boxed, fallible future returned by [`ComposeUpHooks::install_dotfiles`].
///
/// Aliased so the (necessarily verbose) `Pin<Box<dyn Future<Output = Result<…>>>>`
/// shape stays under the `clippy::type_complexity` limit at both the trait
/// definition and the CLI implementation.
pub type DotfilesInstallFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// Callbacks for operations that live outside the orchestrator's dependency
/// graph (daemon management, agent launch, etc.).
pub trait ComposeUpHooks: Send + Sync {
    /// Ensure the daemon is running and return env vars to inject.
    fn daemon_env<'a>(
        &'a self,
        container_name: &'a str,
        host_gateway: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>;

    /// Synchronize daemon connection details into the shared agent volume
    /// (writes `/cella/.daemon_addr`). Mirrors the single-container
    /// `UpHooks::sync_agent_runtime` so an agent can discover a new daemon
    /// address after a daemon restart without relying on its (immutable)
    /// env vars from container creation time.
    fn sync_agent_runtime<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Register a container with the daemon for port management.
    fn register_container<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
        container_id: &'a str,
        config: &'a serde_json::Value,
        container_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Launch the cella-agent as a background process in the container.
    fn launch_agent<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
        container_id: &'a str,
        agent_arch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Run container setup after creation (env injection,
    /// credentials, tool installation, userEnvProbe).
    fn post_create_setup<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
        container_id: &'a str,
        remote_user: &'a str,
        config: &'a serde_json::Value,
        workspace_root: &'a Path,
        remote_env: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>;

    /// Masker over the lifecycle `--secrets-file` values, used to redact
    /// secret values from compose lifecycle command output. Defaults to an
    /// empty (passthrough) masker for implementors without secrets.
    fn lifecycle_secret_masker(&self) -> cella_backend::SecretMasker {
        cella_backend::SecretMasker::default()
    }

    /// Clone and install dotfiles inside the container, as `remote_user`.
    ///
    /// Bridges the orchestrator-owned install logic (which cella-compose cannot
    /// import without a dependency cycle) into the compose flow. Called between
    /// `postCreateCommand` and `postStartCommand`. The returned `Err` is treated
    /// as non-fatal by the caller (logged, never propagated). Defaults to a
    /// no-op so non-CLI implementors (test doubles) need not override it.
    fn install_dotfiles<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
        container_id: &'a str,
        remote_user: &'a str,
        dotfiles: &'a DotfilesConfig,
        lifecycle_env: &'a [String],
    ) -> DotfilesInstallFuture<'a> {
        let _ = (client, container_id, remote_user, dotfiles, lifecycle_env);
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Internal context — bundles repeated arguments for sub-functions
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    client: &'a dyn ContainerBackend,
    cfg: &'a ComposeUpConfig<'a>,
    hooks: &'a dyn ComposeUpHooks,
    progress: &'a ProgressSender,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the Docker Compose orchestration flow.
///
/// # Errors
///
/// Returns an error if any step of the compose pipeline fails.
pub async fn compose_up(
    client: &dyn ContainerBackend,
    cfg: &ComposeUpConfig<'_>,
    hooks: &dyn ComposeUpHooks,
    progress: ProgressSender,
) -> Result<ComposeUpResult, Box<dyn std::error::Error + Send + Sync>> {
    let ctx = Ctx {
        client,
        cfg,
        hooks,
        progress: &progress,
    };
    let config = cfg.config;

    // 1. Build ComposeProject from resolved config
    let mut project = ComposeProject::from_resolved(config, cfg.config_path, cfg.workspace_root)?;
    project.set_compose_options(
        cfg.profiles.clone(),
        cfg.env_files.clone(),
        cfg.pull_policy.clone(),
    );

    info!(
        "Compose project: {} (primary service: {})",
        project.project_name, project.primary_service
    );

    // 2. Validate primary service exists in compose files
    run_step_result(&progress, "Validating compose configuration...", async {
        crate::parse::validate_primary_service(&project.compose_files, &project.primary_service)?;
        if let Some(ref run_services) = project.run_services {
            crate::parse::validate_run_services(&project.compose_files, run_services)?;
        }
        Ok::<(), crate::CellaComposeError>(())
    })
    .await?;

    // 3. Run initializeCommand on host (runs every invocation per spec)
    if let Some(init_cmd) = config.get("initializeCommand") {
        run_host_command("initializeCommand", init_cmd)?;
    }

    // 4. Check for existing compose project
    let existing =
        find_compose_container(client, &project.project_name, &project.primary_service).await?;

    // --expect-existing-container: fail before any build/up if no container
    // exists (matches the official compose path, identical error string). A
    // stopped container counts as existing, so this only fires on a true miss.
    if existing.is_none() && cfg.resolution.expect_existing_container {
        return Err(cella_backend::EXPECTED_CONTAINER_MISSING.into());
    }

    if let Some(ref container) = existing {
        if container.state == ContainerState::Running
            && !cfg.resolution.remove_container
            && !cfg.resolution.build_no_cache
        {
            info!("Compose project already running, running postAttachCommand only");
            return handle_compose_running(&ctx, &project, container).await;
        }

        if cfg.resolution.remove_container || cfg.resolution.build_no_cache {
            run_step_result(&progress, "Stopping existing compose project...", async {
                let (dp, dcp) = cfg.docker_binaries();
                let compose_cmd = ComposeCommand::from_project_name(&project.project_name)
                    .with_docker_binaries(dp, dcp);
                compose_cmd.down().await
            })
            .await?;
        }
    }

    // 5-13. Prepare environment, write override, start services
    let (remote_user, resolved_features, agent_arch, ssh_agent_proxy) =
        prepare_and_start(&ctx, &project).await?;

    // 14-20. Post-start: find container, setup, lifecycle, output
    let mut result = finalize_compose(
        &ctx,
        &project,
        &remote_user,
        resolved_features.as_ref(),
        &agent_arch,
    )
    .await?;
    result.ssh_agent_proxy = ssh_agent_proxy;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Prepare and start (steps 5-13)
// ---------------------------------------------------------------------------

/// Resolve any deferred colima SSH-agent proxy request via the daemon
/// and append the resulting `SSH_AUTH_SOCK` / `CELLA_SSH_AGENT_BRIDGE`
/// / `CELLA_SSH_AGENT_TARGET` env entries to `env_fwd`. Mirrors
/// `Up::resolve_ssh_agent_proxy` for the compose path (compose has no
/// `Self`, so it lives as a free function).
async fn resolve_ssh_agent_proxy_for_compose(
    env_fwd: &mut cella_env::EnvForwarding,
    workspace_root: &Path,
    host_gateway: &str,
) -> Option<SshAgentProxyStatus> {
    let request = env_fwd.ssh_agent_proxy_request.take()?;
    let Some(daemon_sock) = cella_env::paths::daemon_socket_path() else {
        return Some(SshAgentProxyStatus::Skipped {
            reason: "daemon socket path could not be determined".to_string(),
        });
    };
    match cella_daemon_client::ssh_proxy::register_proxy(
        &daemon_sock,
        workspace_root,
        host_gateway,
        &request,
    )
    .await
    {
        Some(resolved) => {
            env_fwd.env.extend(resolved.env);
            Some(SshAgentProxyStatus::Bridged {
                host_endpoint: format!("{host_gateway}:{}", resolved.bridge_port),
                refcount: resolved.refcount,
            })
        }
        None => Some(SshAgentProxyStatus::Skipped {
            reason: "daemon RegisterSshAgentProxy failed (see daemon log)".to_string(),
        }),
    }
}

/// Resolved per-service runtime properties for the compose primary service.
///
/// Bundles the values derived from the resolved features + base image (remote
/// user, base image user, security props, and the merged `devcontainer.metadata`
/// label) together with the env-forwarding plan and ssh-agent proxy status, so
/// `prepare_and_start` threads one value instead of a wide tuple.
struct ComposeRuntimeResolution {
    /// The remote user commands run as.
    remote_user: String,
    /// The base image's USER directive (for the UID-remap layer).
    image_user: String,
    /// Env-forwarding plan (mutated downstream when starting services).
    env_fwd: cella_env::EnvForwarding,
    /// ssh-agent proxy status, surfaced in the up result.
    ssh_agent_proxy: Option<SshAgentProxyStatus>,
    /// Merged security props applied to the primary service.
    security: cella_config::config_map::MergedSecurityConfig,
    /// The `devcontainer.metadata` label value (base image + features + config
    /// entries), stamped on the primary service for tooling interop.
    metadata_label: String,
    /// Feature `entrypoint` scripts in install order, `${devcontainerId}`-
    /// substituted, sourced from the same merged feature/image-metadata config as
    /// the security props (mirrors the single-container `feature_config.entrypoints`).
    feature_entrypoints: Vec<String>,
}

/// Resolve the compose project's remote user, env-forwarding plan, ssh-agent
/// proxy status, security props, and `devcontainer.metadata` label. Extracted
/// from `prepare_and_start` to keep that function under the
/// `clippy::too_many_lines` ceiling.
async fn resolve_user_and_env(
    client: &dyn ContainerBackend,
    ctx: &Ctx<'_>,
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
) -> ComposeRuntimeResolution {
    let cfg = ctx.cfg;
    let progress = ctx.progress;
    let (image_user, image_meta_user, base_image_security, base_metadata) =
        resolve_compose_image_info(
            client,
            project,
            features_build,
            cfg.docker_binaries(),
            progress,
        )
        .await;
    // Runtime security props: prefer the resolved-features config; fall back to
    // the base image's metadata when no features are configured (mirrors the
    // single-container effective_feature_config fallback in up.rs).
    let effective_feature_config = features_build
        .map(|fb| fb.resolved_features.container_config.clone())
        .or(base_image_security);
    let security = cella_config::config_map::merge_security_config(
        cfg.config,
        effective_feature_config.as_ref(),
    );
    // Feature entrypoints (install order), `${devcontainerId}`-substituted, from
    // the same merged config the security props use. Empty when no feature (or
    // base-image-metadata feature) declares an entrypoint, in which case the
    // wrapped entrypoint is skipped unless `overrideCommand` is set.
    let feature_entrypoints = effective_feature_config
        .as_ref()
        .map(|fc| {
            let subst_ctx = cella_config::config_map::subst_ctx(cfg.resolved);
            cella_config::config_map::substitute_feature_config(fc.clone(), &subst_ctx).entrypoints
        })
        .unwrap_or_default();
    // Merged `devcontainer.metadata` label: the features path reuses the label
    // computed during feature resolution; otherwise merge the base image's
    // metadata with the devcontainer.json config entry (mirrors single-container
    // `build_labels`).
    let metadata_label = compose_metadata_label(
        features_build.map(|fb| fb.resolved_features.metadata_label.as_str()),
        cfg.config,
        base_metadata.as_deref(),
        cfg.omit_remote_env_from_metadata,
    );
    let remote_user = resolve_remote_user(cfg.config, image_meta_user.as_ref(), &image_user);
    let managed_agent = client.capabilities().managed_agent;
    let skip_rules = cfg.network_rule_policy == cella_network::NetworkRulePolicy::Skip;
    let proxy_fwd = build_proxy_forwarding_config(cfg.resolved, managed_agent, skip_rules);
    let mut env_fwd =
        cella_env::prepare_env_forwarding(cfg.config, &remote_user, proxy_fwd.as_ref());
    let ssh_agent_proxy = resolve_ssh_agent_proxy_for_compose(
        &mut env_fwd,
        cfg.workspace_root,
        client.host_gateway(),
    )
    .await;
    ComposeRuntimeResolution {
        remote_user,
        image_user,
        env_fwd,
        ssh_agent_proxy,
        security,
        metadata_label,
        feature_entrypoints,
    }
}

/// Prepare environment, write override YAML, and start compose services.
async fn prepare_and_start(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
) -> Result<
    (
        String,
        Option<cella_features::ResolvedFeatures>,
        String,
        Option<SshAgentProxyStatus>,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let (client, cfg, hooks, progress) = (ctx.client, ctx.cfg, ctx.hooks, ctx.progress);
    let config = cfg.config;

    if !client.capabilities().compose {
        return Err(format!(
            "selected backend '{}' does not support Docker Compose devcontainers",
            client.kind()
        )
        .into());
    }

    // 5. Check Docker Compose version supports additional_contexts (>= 2.17.0)
    crate::check_compose_features_support().await?;

    // 6. Resolve features via combined-Dockerfile approach (if features configured)
    let features_build = crate::combined_dockerfile_build::resolve_compose_features(
        client,
        config,
        cfg.config_path,
        project,
        cfg.omit_remote_env_from_metadata,
        cfg.lockfile_policy,
        progress,
    )
    .await?;

    // 6b. Get daemon env vars via hook
    let daemon_env = hooks
        .daemon_env(cfg.container_name, client.host_gateway())
        .await;

    // 7. Detect container architecture and ensure agent volume is populated
    let agent_arch = client.detect_container_arch().await.unwrap_or_else(|e| {
        warn!("Failed to detect container arch, defaulting to x86_64: {e}");
        "x86_64".to_string()
    });

    let (agent_vol_name, agent_vol_target, _) = if client.capabilities().managed_agent {
        let version = env!("CARGO_PKG_VERSION");
        run_step_result(progress, "Preparing agent volume...", async {
            client
                .ensure_agent_provisioned(version, &agent_arch, cfg.skip_checksum)
                .await
        })
        .await?;
        client.agent_volume_mount()
    } else {
        (String::new(), String::new(), true)
    };

    // 8. Write initial build override (features Dockerfile only, labels/env added later)
    let build_ov = OverrideContext {
        agent_vol_name: agent_vol_name.clone(),
        agent_vol_target: agent_vol_target.clone(),
        extra_env: Vec::new(),
        labels: BTreeMap::new(),
        extra_volumes: Vec::new(),
        // GPU reservation is emitted only in the final override, not at build.
        request_gpu: false,
        // Runtime security props are applied in the final override, not at build.
        security: cella_config::config_map::MergedSecurityConfig::default(),
        // Feature entrypoints + entrypoint/command resolution are emitted only in
        // the final override; the build override never runs the container.
        feature_entrypoints: Vec::new(),
        user_entrypoint_command: crate::override_file::UserEntrypointCommand::default(),
    };
    write_build_override(project, features_build.as_ref(), &build_ov)?;

    // 9. Run docker compose build to ensure images exist for inspection.
    let (dp, dcp) = ctx.cfg.docker_binaries();
    let compose_cmd = ComposeCommand::new(project).with_docker_binaries(dp, dcp);
    run_step_result(
        progress,
        "Building compose services...",
        compose_cmd.build(None, cfg.resolution.build_no_cache),
    )
    .await?;

    // 10. Resolve remote user, env forwarding, and ssh-agent proxy.
    let ComposeRuntimeResolution {
        remote_user,
        image_user,
        mut env_fwd,
        ssh_agent_proxy,
        security,
        metadata_label,
        feature_entrypoints,
    } = resolve_user_and_env(client, ctx, project, features_build.as_ref()).await;
    info!("Resolved remote user: {remote_user} (image user: {image_user})");

    // 11-15. Build override context, UID remap, write override, and start.
    build_override_and_start(BuildAndStartParams {
        ctx,
        compose_cmd: &compose_cmd,
        project,
        features_build: features_build.as_ref(),
        config,
        daemon_env,
        env_fwd: &mut env_fwd,
        remote_user: &remote_user,
        image_user: &image_user,
        agent_vol_name,
        agent_vol_target,
        security,
        metadata_label,
        feature_entrypoints,
    })
    .await?;

    let resolved_features = features_build.map(|b| b.resolved_features);
    Ok((remote_user, resolved_features, agent_arch, ssh_agent_proxy))
}

// ---------------------------------------------------------------------------
// Finalize (steps 14-20)
// ---------------------------------------------------------------------------

/// Find primary container, run post-create setup, lifecycle phases, and
/// return the result.
async fn finalize_compose(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
    remote_user: &str,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    agent_arch: &str,
) -> Result<ComposeUpResult, Box<dyn std::error::Error + Send + Sync>> {
    let (client, cfg, hooks, progress) = (ctx.client, ctx.cfg, ctx.hooks, ctx.progress);
    let config = cfg.config;

    // 14. Find primary container via compose labels
    let primary = find_compose_container(client, &project.project_name, &project.primary_service)
        .await?
        .ok_or("Primary service container not found after docker compose up")?;

    info!(
        "Primary container: {} ({})",
        primary.name,
        &primary.id[..12.min(primary.id.len())]
    );

    // 15. Verify primary container is running
    verify_container_running(client, &primary.id).await?;

    // 15b. Connect to cella network for daemon port-forwarding reachability
    if let Err(e) = client
        .ensure_container_network(&primary.id, cfg.workspace_root)
        .await
    {
        warn!("Failed to connect container to cella network: {e}");
    }

    // 16. Register with daemon (primary container only)
    hooks
        .register_container(client, &primary.id, config, cfg.container_name)
        .await;

    // 17. Post-create setup (UID, env, credentials, tools, userEnvProbe).
    let lifecycle_remote_env = cfg.lifecycle_remote_env();
    let lifecycle_env = hooks
        .post_create_setup(
            client,
            &primary.id,
            remote_user,
            config,
            cfg.workspace_root,
            &lifecycle_remote_env,
        )
        .await;

    // 18. Publish daemon address to the shared agent volume before the
    // agent starts reading it. Without this, the agent is left relying on
    // the container's (immutable) env vars and has no way to learn about
    // a daemon restart that changed the port.
    hooks.sync_agent_runtime(client).await;

    // 19. Launch agent as background process via exec
    hooks.launch_agent(client, &primary.id, agent_arch).await;

    // 20. Run lifecycle phases (primary service only). Honor the lifecycle
    // gate: --skip-post-create drops everything, --prebuild and
    // --skip-non-blocking-commands stop after the waitFor phase, and
    // --skip-post-attach drops only postAttachCommand. Compose runs phases
    // sequentially in the foreground (no backgrounding), so the gate's
    // "does this phase run?" decision is all that applies here.
    let gate = cfg.lifecycle_gate;
    let metadata = resolved_features.map(|rf| rf.metadata_label.as_str());
    let subst_ctx = cella_config::config_map::subst_ctx(cfg.resolved);
    let secret_masker = hooks.lifecycle_secret_masker();
    for phase in [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ] {
        if !gate.runs_phase(phase) {
            continue;
        }
        let mut entries = lifecycle_entries_for_phase(metadata, config, phase);
        cella_config::config_map::substitute_lifecycle_entries(&mut entries, &subst_ctx);
        let lc_ctx = build_lifecycle_ctx(
            client,
            &primary.id,
            remote_user,
            &lifecycle_env,
            Some(&project.workspace_folder),
            progress,
            secret_masker.clone(),
        );
        run_lifecycle_entries(&lc_ctx, phase, &entries, progress).await?;

        // Dotfiles install runs after postCreateCommand but past the postCreate
        // skipNonBlocking checkpoint (official injectHeadless.ts:392, after the
        // :388 return). Gate on postStartCommand (not postCreateCommand) so
        // `--skip-non-blocking-commands` with `waitFor: postCreateCommand`
        // correctly skips dotfiles. Non-fatal: a failure warns but never fails `up`.
        if phase == "postCreateCommand"
            && cfg.dotfiles.repository.is_some()
            && gate.runs_phase("postStartCommand")
        {
            install_dotfiles_step(ctx, &primary.id, remote_user, &lifecycle_env).await;
        }
    }

    Ok(ComposeUpResult {
        container_id: primary.id,
        remote_user: remote_user.to_string(),
        workspace_folder: project.workspace_folder.clone(),
        project_name: project.project_name.clone(),
        outcome: ComposeUpOutcome::Created,
        ssh_agent_proxy: None,
    })
}

/// Run the dotfiles install hook with progress reporting (non-fatal on error).
///
/// A dotfiles failure is logged and surfaced as a progress warning but never
/// propagated, so `up` still succeeds — matching the official tool.
async fn install_dotfiles_step(
    ctx: &Ctx<'_>,
    container_id: &str,
    remote_user: &str,
    lifecycle_env: &[String],
) {
    let (client, cfg, hooks, progress) = (ctx.client, ctx.cfg, ctx.hooks, ctx.progress);
    let step = progress.step("Installing dotfiles...");
    match hooks
        .install_dotfiles(
            client,
            container_id,
            remote_user,
            &cfg.dotfiles,
            lifecycle_env,
        )
        .await
    {
        Ok(()) => step.finish(),
        Err(e) => {
            // The dotfiles script runs with the secret-bearing lifecycle_env;
            // its failure message can echo stderr that contains secret values,
            // so mask before logging or surfacing to the terminal.
            let err_text = e.to_string();
            let msg = hooks.lifecycle_secret_masker().mask(&err_text);
            warn!("Dotfiles install failed (continuing): {msg}");
            step.fail("failed");
            progress.warn(&format!("Dotfiles install failed (continuing): {msg}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Handle already-running
// ---------------------------------------------------------------------------

/// Handle a compose project that's already running — just run postAttachCommand.
async fn handle_compose_running(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
    container: &ContainerInfo,
) -> Result<ComposeUpResult, Box<dyn std::error::Error + Send + Sync>> {
    let (client, cfg, hooks, progress) = (ctx.client, ctx.cfg, ctx.hooks, ctx.progress);
    let config = cfg.config;

    // Prefer the label stored during creation; fall back to config resolution.
    let remote_user = container
        .labels
        .get("dev.cella.remote_user")
        .filter(|u| u.as_str() != "root")
        .cloned()
        .unwrap_or_else(|| resolve_remote_user(config, None, "root"));

    // Warn on config-hash drift (mirrors single-container warn_config_drift at up.rs:486-508).
    let current_hash = project.config_hash.as_str();
    let old_hash: Option<&str> = container.config_hash.as_deref().or_else(|| {
        container
            .labels
            .get("dev.cella.config_hash")
            .map(String::as_str)
    });
    if let Some(old) = old_hash
        && old != current_hash
    {
        progress.warn("Config has changed since this container was created.");
        progress.hint("Run `cella up --rebuild` to recreate with the updated config.");
    }

    // Runtime drift.
    let current_runtime = cella_env::platform::detect_runtime().as_label();
    if let Some(old_runtime) = container.labels.get("dev.cella.docker_runtime")
        && old_runtime != current_runtime
    {
        progress.warn(&format!(
            "Docker runtime changed ({old_runtime} -> {current_runtime})."
        ));
        progress.hint("Run `cella up --rebuild` to recreate with the updated runtime.");
    }

    // Mount-input drift (settings, env forwarding, parent-git) — catches
    // mount-affecting changes that `config_hash` does not cover.
    let env_fwd_now = cella_env::prepare_env_forwarding(config, &remote_user, None);
    let settings_now = cella_config::CellaConfig::load(cfg.workspace_root, Some(cfg.resolved))?;
    let current_mount_fp = crate::mount_parity::compute_mount_input_fingerprint(
        &settings_now,
        &env_fwd_now,
        cfg.workspace_root,
    );
    if let Some(old_fp) = container.labels.get("dev.cella.mount_input_fingerprint")
        && old_fp != &current_mount_fp
    {
        progress.warn("Mount configuration has changed since this container was created.");
        progress.hint("Run `cella up --rebuild` to recreate with the updated mounts.");
    }

    // Ensure container is on cella network (may have been created before this was added)
    if let Err(e) = client
        .ensure_container_network(&container.id, cfg.workspace_root)
        .await
    {
        warn!("Failed to connect container to cella network: {e}");
    }

    // Re-register with daemon in case it restarted
    hooks
        .register_container(client, &container.id, config, cfg.container_name)
        .await;

    // Refresh `.daemon_addr` on the shared volume so any agent still
    // running inside the already-up container can follow a daemon port
    // change since the last `cella up` (e.g., daemon binary update).
    hooks.sync_agent_runtime(client).await;

    // Restart the agent so it reconnects and re-reports ports with the
    // (potentially updated) cella network IP.
    restart_agent_in_container(client, &container.id).await;

    // postAttachCommand runs on every attach to an already-running compose
    // project. Honor the gate (--skip-post-create / --skip-post-attach /
    // stop-after flags all suppress it).
    if cfg.lifecycle_gate.runs_post_attach()
        && let Some(cmd) = config.get("postAttachCommand")
        && !cmd.is_null()
    {
        let lifecycle_env = cfg.lifecycle_remote_env();
        let lc_ctx = build_lifecycle_ctx(
            client,
            &container.id,
            &remote_user,
            &lifecycle_env,
            Some(project.workspace_folder.as_str()),
            progress,
            hooks.lifecycle_secret_masker(),
        );

        let label = "Running the postAttachCommand from devcontainer.json...";
        progress.println(&format!("  \x1b[36m\u{25b8}\x1b[0m {label}"));
        let result =
            run_lifecycle_phase(&lc_ctx, "postAttachCommand", cmd, "devcontainer.json").await;
        match &result {
            Ok(()) => progress.println(&format!("  \x1b[32m\u{2713}\x1b[0m {label}")),
            Err(e) => progress.println(&format!("  \x1b[31m\u{2717}\x1b[0m {label}: {e}")),
        }
        result?;
    }

    Ok(ComposeUpResult {
        container_id: container.id.clone(),
        remote_user,
        workspace_folder: project.workspace_folder.clone(),
        project_name: project.project_name.clone(),
        outcome: ComposeUpOutcome::Running,
        ssh_agent_proxy: None,
    })
}

/// Compose override context shared between build and UID remap override writes.
struct OverrideContext {
    agent_vol_name: String,
    agent_vol_target: String,
    extra_env: Vec<String>,
    labels: BTreeMap<String, String>,
    extra_volumes: Vec<MountSpec>,
    /// Whether to emit the GPU reservation block (config requires a GPU AND
    /// `--gpu-availability` grants it). Net-new compose GPU support.
    request_gpu: bool,
    /// Merged container security/runtime properties (containerUser, init,
    /// privileged, capAdd, securityOpt) applied to the primary service.
    security: cella_config::config_map::MergedSecurityConfig,
    /// Feature `entrypoint` scripts (install order, `${devcontainerId}`-
    /// substituted) to run before the service's entrypoint+command in the wrapped
    /// entrypoint. Read only by `write_final_override` (the runtime override); the
    /// build override leaves entrypoints empty.
    feature_entrypoints: Vec<String>,
    /// Resolved `userEntrypoint`/`userCommand` for the wrapped entrypoint (per the
    /// official compose logic). Read only by `write_final_override`.
    user_entrypoint_command: crate::override_file::UserEntrypointCommand,
}

// ---------------------------------------------------------------------------
// Override helpers
// ---------------------------------------------------------------------------

/// Write the initial compose override YAML for building with features.
fn write_build_override(
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
    ov: &OverrideContext,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let override_config = OverrideConfig {
        primary_service: project.primary_service.clone(),
        image_override: features_build.and_then(|b| b.image_name_override.clone()),
        override_command: project.override_command,
        agent_volume_name: ov.agent_vol_name.clone(),
        agent_volume_target: ov.agent_vol_target.clone(),
        extra_env: ov.extra_env.clone(),
        extra_labels: ov.labels.clone(),
        build_dockerfile: features_build.map(|b| b.combined_dockerfile.clone()),
        build_target: features_build.map(|b| b.build_target.clone()),
        build_context: features_build.and_then(|b| b.build_context.clone()),
        additional_contexts: features_build
            .map(|b| b.additional_contexts.clone())
            .unwrap_or_default(),
        build_secrets: Vec::new(),
        // `up` does not thread `cella build --label`; image labels are a
        // build-only concern. Keep empty so the `up` override is unchanged.
        build_labels: Vec::new(),
        extra_volumes: Vec::new(),
        // The build-time override never needs the GPU reservation; it is
        // emitted only in the final override used for `compose up`.
        request_gpu: false,
        // build_ov carries default (empty) security props at build time.
        security: ov.security.clone(),
        // Feature entrypoints + entrypoint/command resolution are emitted only in
        // the final override; this build override is overwritten by
        // `write_final_override` before `compose up`.
        feature_entrypoints: Vec::new(),
        user_entrypoint: Vec::new(),
        user_command: None,
        // The `up` flow provisions the agent volume during setup and reuses this
        // override at runtime, so keep the runtime sections (agent volume).
        build_only: false,
    };
    let override_yaml = crate::override_file::generate_override_yaml(&override_config);
    crate::override_file::write_override_file(&project.override_file, &override_yaml)?;
    debug!(
        "Override file written to: {}",
        project.override_file.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Image metadata resolution
// ---------------------------------------------------------------------------

/// Resolve the image user and metadata from the compose service's built image.
///
/// For image-only services, inspects the image directly (pulling if needed).
/// For build-based services, inspects the compose-built image (`{project}-{service}`).
///
/// Returns `(image_user, Option<ImageMetadataUserInfo>, Option<FeatureContainerConfig>, raw_metadata)`.
/// The third element is the base image's `devcontainer.metadata` container config,
/// used for runtime security props when no features are configured (mirrors the
/// single-container base-image-metadata fallback in `up.rs`). The fourth is the
/// RAW `devcontainer.metadata` label string from the base image, merged into the
/// `devcontainer.metadata` label stamped on the primary service. Falls back to
/// `("root", None, None, None)` when inspection fails.
async fn resolve_compose_image_info(
    client: &dyn ContainerBackend,
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
    docker_binaries: (Option<String>, Option<String>),
    progress: &ProgressSender,
) -> (
    String,
    Option<cella_features::ImageMetadataUserInfo>,
    Option<cella_features::FeatureContainerConfig>,
    Option<String>,
) {
    // If features resolved an image, its metadata was already extracted.
    if let Some(fb) = features_build {
        let meta_user = fb
            .base_image_metadata
            .as_deref()
            .map(|m| cella_features::parse_image_metadata(m).1);
        return (
            fb.image_user.clone(),
            meta_user,
            None,
            fb.base_image_metadata.clone(),
        );
    }

    // Resolve compose config to find the service's image source.
    let (dp, dcp) = docker_binaries;
    let compose_cmd = ComposeCommand::without_override(project).with_docker_binaries(dp, dcp);
    let resolved = match compose_cmd.config().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to resolve compose config for image metadata: {e}");
            return ("root".to_string(), None, None, None);
        }
    };

    let service_info = match crate::extract_service_build_info(&resolved, &project.primary_service)
    {
        Ok(info) => info,
        Err(e) => {
            warn!("Failed to extract service build info: {e}");
            return ("root".to_string(), None, None, None);
        }
    };

    // For an image-only service, pull it if not locally available so the
    // following inspect can read its metadata.
    if let ServiceBuildInfo::Image { image } = &service_info
        && matches!(client.image_exists(image).await, Ok(false))
    {
        let _ = run_step_result(
            progress,
            "Pulling compose service image...",
            client.pull_image(image),
        )
        .await;
    }
    let image_name =
        service_info.resolved_image_name(&project.project_name, &project.primary_service);

    match client.inspect_image_details(&image_name).await {
        Ok(details) => match details.metadata {
            // Keep the raw label string for the merged metadata label; the parsed
            // form only carries security/user props.
            Some(meta) => {
                let (cfg, user) = cella_features::parse_image_metadata(&meta);
                (details.user, Some(user), Some(cfg), Some(meta))
            }
            None => (details.user, None, None, None),
        },
        Err(e) => {
            warn!("Failed to inspect image '{image_name}' for metadata: {e}");
            ("root".to_string(), None, None, None)
        }
    }
}

/// Resolve the wrapped entrypoint's `userEntrypoint`/`userCommand` for the
/// primary service, mirroring the official compose logic.
///
/// Reads the service's resolved `entrypoint`/`command` from `docker compose
/// config` and the base image's `ENTRYPOINT`/`CMD` from an inspect, then applies
/// [`crate::override_file::resolve_user_entrypoint_command`]. Falls back to the
/// default (empty entrypoint, no command) when the compose config can't be
/// resolved — the wrapped entrypoint then just runs feature entrypoints and
/// `exec "$@"` over whatever the service already had.
async fn resolve_compose_user_entrypoint_command(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
) -> crate::override_file::UserEntrypointCommand {
    // overrideCommand discards the service's original entrypoint+command, so the
    // result is fixed regardless of the compose config or image — skip resolving
    // the config and inspecting the image (the official CLI is lazy here too).
    if project.override_command {
        return crate::override_file::resolve_user_entrypoint_command(true, None, None, &[], &[]);
    }

    let (dp, dcp) = ctx.cfg.docker_binaries();
    let compose_cmd = ComposeCommand::without_override(project).with_docker_binaries(dp, dcp);
    let resolved = match compose_cmd.config().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to resolve compose config for entrypoint/command: {e}");
            return crate::override_file::UserEntrypointCommand::default();
        }
    };

    let (compose_entrypoint, compose_command) =
        match crate::extract_service_entrypoint_command(&resolved, &project.primary_service) {
            Ok(pair) => pair,
            Err(e) => {
                warn!("Failed to extract service entrypoint/command: {e}");
                (None, None)
            }
        };

    // The base image to inspect for the ENTRYPOINT/CMD fallback: the
    // features-build override if present, otherwise the service's resolved image.
    let image_name = features_build
        .and_then(|b| b.image_name_override.clone())
        .or_else(|| {
            crate::extract_service_build_info(&resolved, &project.primary_service)
                .ok()
                .map(|info| {
                    info.resolved_image_name(&project.project_name, &project.primary_service)
                })
        });

    let (image_entrypoint, image_cmd) = match image_name {
        Some(name) => match ctx.client.inspect_image_details(&name).await {
            Ok(details) => (details.entrypoint, details.cmd),
            Err(e) => {
                warn!("Failed to inspect image '{name}' for entrypoint/cmd: {e}");
                (Vec::new(), Vec::new())
            }
        },
        None => (Vec::new(), Vec::new()),
    };

    crate::override_file::resolve_user_entrypoint_command(
        project.override_command,
        compose_entrypoint.as_deref(),
        compose_command.as_deref(),
        &image_entrypoint,
        &image_cmd,
    )
}

// ---------------------------------------------------------------------------
// UID remap
// ---------------------------------------------------------------------------

/// Whether the compose service should be granted a GPU.
///
/// Mirrors the single-container gate: the config must request a GPU
/// (`hostRequirements.gpu` truthy) AND `--gpu-availability` must grant it
/// (`all` => always, `none` => never, `detect` => daemon probe). When the
/// config requires a GPU but support is declined, the official warning is
/// emitted (unless the requirement was marked `optional`).
async fn resolve_compose_gpu(ctx: &Ctx<'_>, config: &serde_json::Value) -> bool {
    let gpu = config.get("hostRequirements").and_then(|h| h.get("gpu"));
    let requires_gpu = matches!(
        gpu,
        Some(serde_json::Value::Bool(true) | serde_json::Value::Object(_))
    ) || gpu.and_then(serde_json::Value::as_str) == Some("optional");
    if !requires_gpu {
        return false;
    }

    let supported = match ctx.cfg.gpu_availability {
        cella_backend::GpuAvailability::All => true,
        cella_backend::GpuAvailability::None => false,
        cella_backend::GpuAvailability::Detect => {
            ctx.client.detect_gpu_support().await.unwrap_or(false)
        }
    };

    if !supported {
        let is_optional = gpu.and_then(serde_json::Value::as_str) == Some("optional");
        if !is_optional {
            ctx.progress.warn(
                "No GPU support found yet a GPU was required - consider marking it as \"optional\"",
            );
        }
    }
    supported
}

/// Build a UID-remapped image for the compose service.
///
/// Returns the UID-remapped image name, or `None` if remap was skipped.
///
/// The platform (`--platform`) is resolved internally by
/// [`cella_backend::uid_image::build_uid_remap_image`] via image inspection,
/// covering both image-only and features-build paths correctly.
async fn build_uid_remap_image_compose(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
    remote_user: &str,
    image_user: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let config_value = ctx
        .cfg
        .config
        .get("updateRemoteUserUID")
        .and_then(serde_json::Value::as_bool);
    let update_uid =
        cella_backend::should_update_uid(config_value, ctx.cfg.update_remote_user_uid_default);

    if !update_uid {
        return Ok(None);
    }

    // compose_image is the ACTUAL image that will be used as the remap base:
    // either the features-build image override or the default {project}-{service}.
    // build_uid_remap_image inspects THIS image to derive --platform, so the
    // compose+features path (features_build is Some) gets the correct platform.
    let compose_image = features_build
        .and_then(|b| b.image_name_override.clone())
        .unwrap_or_else(|| format!("{}-{}", project.project_name, project.primary_service));

    cella_backend::uid_image::build_uid_remap_image(
        ctx.client,
        &compose_image,
        image_user,
        remote_user,
        cella_backend::uid_image::BuildToolchain {
            docker_path: ctx.cfg.build_tuning.docker_path.as_deref(),
            use_buildkit: ctx.cfg.build_tuning.use_buildkit,
        },
        ctx.progress,
    )
    .await
}

/// Write the final compose override with labels, env, and optional UID remap image.
fn write_final_override(
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
    ov: &OverrideContext,
    uid_image: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let image_override =
        uid_image.or_else(|| features_build.and_then(|b| b.image_name_override.clone()));

    let override_config = OverrideConfig {
        primary_service: project.primary_service.clone(),
        image_override,
        override_command: project.override_command,
        agent_volume_name: ov.agent_vol_name.clone(),
        agent_volume_target: ov.agent_vol_target.clone(),
        extra_env: ov.extra_env.clone(),
        extra_labels: ov.labels.clone(),
        build_dockerfile: features_build.map(|b| b.combined_dockerfile.clone()),
        build_target: features_build.map(|b| b.build_target.clone()),
        build_context: features_build.and_then(|b| b.build_context.clone()),
        additional_contexts: features_build
            .map(|b| b.additional_contexts.clone())
            .unwrap_or_default(),
        build_secrets: Vec::new(),
        // `up` does not thread `cella build --label`; image labels are a
        // build-only concern. Keep empty so the `up` override is unchanged.
        build_labels: Vec::new(),
        extra_volumes: ov.extra_volumes.clone(),
        request_gpu: ov.request_gpu,
        security: ov.security.clone(),
        // Wrapped entrypoint: feature entrypoints run before the service's
        // original entrypoint+command is exec'd (resolved in build_override_and_start).
        feature_entrypoints: ov.feature_entrypoints.clone(),
        user_entrypoint: ov.user_entrypoint_command.entrypoint.clone(),
        user_command: ov.user_entrypoint_command.command.clone(),
        // The final `up` override runs the container; keep the runtime sections.
        build_only: false,
    };
    let override_yaml = crate::override_file::generate_override_yaml(&override_config);
    crate::override_file::write_override_file(&project.override_file, &override_yaml)?;
    debug!(
        "Final override written to: {}",
        project.override_file.display()
    );
    Ok(())
}

/// Inputs to [`build_override_and_start`], bundled to keep the call under the
/// `clippy::too_many_arguments` ceiling.
struct BuildAndStartParams<'a> {
    ctx: &'a Ctx<'a>,
    compose_cmd: &'a ComposeCommand,
    project: &'a ComposeProject,
    features_build: Option<&'a crate::combined_dockerfile_build::ComposeFeaturesBuild>,
    config: &'a serde_json::Value,
    daemon_env: Vec<String>,
    env_fwd: &'a mut cella_env::EnvForwarding,
    remote_user: &'a str,
    image_user: &'a str,
    agent_vol_name: String,
    agent_vol_target: String,
    security: cella_config::config_map::MergedSecurityConfig,
    metadata_label: String,
    /// Feature entrypoints (install order, substituted) for the wrapped entrypoint.
    feature_entrypoints: Vec<String>,
}

/// Build the override context (env, labels, mounts), UID remap image,
/// and start compose services with SSH fallback retry.
async fn build_override_and_start(
    params: BuildAndStartParams<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let BuildAndStartParams {
        ctx,
        compose_cmd,
        project,
        features_build,
        config,
        daemon_env,
        env_fwd,
        remote_user,
        image_user,
        agent_vol_name,
        agent_vol_target,
        security,
        metadata_label,
        feature_entrypoints,
    } = params;
    let cfg = ctx.cfg;
    let progress = ctx.progress;

    let managed = ctx.client.capabilities().managed_agent;
    let mut extra_env = build_extra_env(daemon_env, env_fwd, cfg.remote_env, managed);
    // When `remoteEnv` contains `${containerEnv:VAR}` tokens, phase-1
    // substitution collapses them to `""` (container env is unavailable at
    // parse time).  Injecting those phase-1 values into the compose service
    // environment pollutes the `userEnvProbe` result: the probe reads e.g.
    // `PATH=:/opt/bin` instead of the real image PATH, so the subsequent
    // second-pass resolution in `build_lifecycle_env` expands
    // `${containerEnv:PATH}` from the polluted value and produces
    // `:/opt/bin:/opt/bin` instead of `<real_PATH>:/opt/bin`.
    //
    // Guard: only strip when the raw snapshot actually has tokens — the
    // common case (no `containerEnv` references) is unaffected.
    strip_container_env_polluted_entries(&mut extra_env, cfg.resolved.raw_remote_env.as_ref());
    let mut labels = build_compose_labels(cfg, project, remote_user);
    // Stamp the merged `devcontainer.metadata` label (base image + features +
    // config entries) so tooling can reconstruct the merged config from the
    // running primary service, matching the single-container path.
    labels.insert("devcontainer.metadata".to_string(), metadata_label);

    let settings = cella_config::CellaConfig::load(cfg.workspace_root, Some(cfg.resolved))?;
    cella_tool_install::ensure_tool_config_paths(&settings);
    // Container env is immutable after create, so the claude.json sync opt-in
    // must be baked into the compose override here (mirrors the single-container
    // `apply_env_and_mounts` injection).
    extra_env.extend(cella_tool_install::tool_config_env_vars(
        &settings,
        remote_user,
    ));
    insert_mount_input_fingerprint_label(&mut labels, &settings, env_fwd, cfg.workspace_root);

    let subst_ctx = cella_config::config_map::subst_ctx(cfg.resolved);
    let mount_specs = build_compose_mount_specs(ComposeMountParams {
        workspace_root: cfg.workspace_root,
        settings: &settings,
        remote_user,
        env_fwd,
        project,
        config,
        resolved_features: features_build.map(|fb| &fb.resolved_features),
        subst_ctx: &subst_ctx,
        agent_vol_target: &agent_vol_target,
        agent_vol_name: &agent_vol_name,
        docker_binaries: cfg.docker_binaries(),
    })
    .await?;

    let request_gpu = resolve_compose_gpu(ctx, config).await;

    // Resolve the wrapped entrypoint's userEntrypoint/userCommand: the service's
    // own entrypoint/command (from the resolved compose config) falling back to
    // the image's ENTRYPOINT/CMD, gated by `overrideCommand`. Skipped (default —
    // empty/None) when there are no feature entrypoints and the command is not
    // overridden, so the no-feature override stays byte-for-byte unchanged.
    let user_entrypoint_command = if feature_entrypoints.is_empty() && !project.override_command {
        crate::override_file::UserEntrypointCommand::default()
    } else {
        resolve_compose_user_entrypoint_command(ctx, project, features_build).await
    };

    let mut ov_ctx = OverrideContext {
        agent_vol_name,
        agent_vol_target,
        extra_env,
        labels,
        extra_volumes: mount_specs,
        request_gpu,
        security,
        feature_entrypoints,
        user_entrypoint_command,
    };

    let uid_image =
        build_uid_remap_image_compose(ctx, project, features_build, remote_user, image_user)
            .await?;

    compose_up_with_ssh_fallback(
        compose_cmd,
        project,
        features_build,
        &mut ov_ctx,
        uid_image,
        env_fwd,
        progress,
    )
    .await
}

/// Write override and run `compose up`, retrying on mount failures.
///
/// SSH agent mount failures cycle through fallback strategies then skip
/// SSH forwarding. Non-SSH bind mount failures (transient TOCTOU races)
/// are retried up to 3 times with a 500ms delay.
async fn compose_up_with_ssh_fallback(
    compose_cmd: &ComposeCommand,
    project: &ComposeProject,
    features_build: Option<&crate::combined_dockerfile_build::ComposeFeaturesBuild>,
    ov_ctx: &mut OverrideContext,
    uid_image: Option<String>,
    env_fwd: &mut cella_env::EnvForwarding,
    progress: &ProgressSender,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const MAX_BIND_MOUNT_RETRIES: u32 = 3;
    let mut bind_mount_attempts: u32 = 0;

    loop {
        write_final_override(project, features_build, ov_ctx, uid_image.clone())?;

        let result: Result<(), Box<dyn std::error::Error + Send + Sync>> =
            run_step_result(progress, "Starting compose services...", async {
                compose_cmd.up(project.run_services.as_deref(), false).await
            })
            .await
            .map_err(Into::into);

        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let err_msg = e.to_string();

                if cella_env::ssh_agent::is_ssh_mount_error(
                    &err_msg,
                    env_fwd.ssh_agent_mount_source.as_deref(),
                ) {
                    if let Some(ref source) = env_fwd.ssh_agent_mount_source {
                        ov_ctx.extra_volumes.retain(|m| m.source != *source);
                        env_fwd.mounts.retain(|m| m.source != *source);
                    }
                    ov_ctx
                        .extra_env
                        .retain(|e| !e.starts_with("SSH_AUTH_SOCK="));
                    env_fwd.env.retain(|e| e.key != "SSH_AUTH_SOCK");
                    env_fwd.ssh_agent_mount_source = None;

                    if let Some(next) = env_fwd.ssh_agent_fallbacks.first().cloned() {
                        env_fwd.ssh_agent_fallbacks.remove(0);
                        match next {
                            cella_env::ssh_agent::SshAgentRequest::Direct(ssh) => {
                                info!(
                                    "Compose SSH mount failed, trying fallback: {}",
                                    ssh.mount_source
                                );
                                env_fwd.ssh_agent_mount_source = Some(ssh.mount_source.clone());
                                ov_ctx
                                    .extra_volumes
                                    .push(MountSpec::bind(ssh.mount_source, ssh.mount_target));
                                ov_ctx
                                    .extra_env
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
                    progress.warn(&cella_env::ssh_agent::ssh_skip_warning(runtime));

                    write_final_override(project, features_build, ov_ctx, uid_image)?;
                    return run_step_result(progress, "Starting compose services...", async {
                        compose_cmd.up(project.run_services.as_deref(), false).await
                    })
                    .await
                    .map_err(Into::into);
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

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

/// The `devcontainer.metadata` label value for the compose primary service.
///
/// Mirrors the single-container `build_labels` logic: when features were
/// resolved, reuse the label computed during feature resolution (it already
/// merges the base image, feature, and config entries). Otherwise build it from
/// the base image's metadata (if any) plus the devcontainer.json config entry —
/// exactly what the official CLI's `getDevcontainerMetadata` writes.
fn compose_metadata_label(
    features_metadata_label: Option<&str>,
    config: &serde_json::Value,
    base_metadata: Option<&str>,
    omit_remote_env: bool,
) -> String {
    features_metadata_label.map_or_else(
        || cella_features::generate_metadata_label(&[], config, base_metadata, omit_remote_env),
        ToString::to_string,
    )
}

/// Build cella labels for the compose override file.
///
/// Includes both cella-specific and spec-standard labels for VS Code interop.
fn build_compose_labels(
    cfg: &ComposeUpConfig<'_>,
    project: &ComposeProject,
    remote_user: &str,
) -> BTreeMap<String, String> {
    // Use lexical (non-symlink-resolving) paths to match the values hashed by
    // `devcontainer_id` and by VS Code / the official CLI. Canonicalization
    // would resolve symlinks (e.g. macOS /tmp → /private/tmp, bind mounts) and
    // make the labels disagree with the ID in symlinked-workspace scenarios.
    let workspace_str = lexical_absolute(cfg.workspace_root)
        .to_string_lossy()
        .to_string();
    let config_str = lexical_absolute(cfg.config_path)
        .to_string_lossy()
        .to_string();

    let mut labels = BTreeMap::new();

    // Cella-specific labels.
    labels.insert("dev.cella.tool".to_string(), "cella".to_string());
    // `dev.cella.version` mirrors what the non-compose path stamps at
    // up.rs:686. Two existing readers consult it:
    //   - cella-doctor's check_version_skew falls back to this label when
    //     the agent isn't connected; without it, compose containers show
    //     "container unknown != CLI <ver>" every time the daemon restarts.
    //   - cella-orchestrator's up.rs uses it to detect version skew and
    //     decide whether to repopulate the agent volume on `cella up`.
    // Both readers treat a missing label as "unknown", so this is a
    // best-effort fallback, not an authoritative source of truth.
    labels.insert(
        "dev.cella.version".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    labels.insert(
        "dev.cella.workspace_path".to_string(),
        workspace_str.clone(),
    );
    labels.insert("dev.cella.config_path".to_string(), config_str.clone());
    labels.insert(
        "dev.cella.config_hash".to_string(),
        project.config_hash.clone(),
    );
    labels.insert(
        "dev.cella.compose_project".to_string(),
        project.project_name.clone(),
    );
    labels.insert(
        "dev.cella.primary_service".to_string(),
        project.primary_service.clone(),
    );
    labels.insert("dev.cella.remote_user".to_string(), remote_user.to_string());
    labels.insert(
        "dev.cella.workspace_folder".to_string(),
        project.workspace_folder.clone(),
    );
    labels.insert(
        "dev.cella.docker_runtime".to_string(),
        cella_env::platform::detect_runtime().as_label().to_string(),
    );
    labels.insert(
        "dev.cella.user_env_probe".to_string(),
        cfg.user_env_probe.to_string(),
    );

    // Spec-standard labels for VS Code / tooling interop.
    labels.insert("devcontainer.local_folder".to_string(), workspace_str);
    labels.insert("devcontainer.config_file".to_string(), config_str);

    labels
}

/// Assemble the extra environment variable list for the compose service.
///
/// Combines daemon-injected env vars, forwarded env vars (SSH/GPG agent sockets,
/// etc.), user-specified `remote_env` overrides, and — when the backend has a
/// managed agent — the agent env vars (notably `BROWSER=/cella/bin/cella-browser`)
/// in precedence order.
fn build_extra_env(
    daemon_env: Vec<String>,
    env_fwd: &cella_env::EnvForwarding,
    remote_env: &[String],
    managed_agent: bool,
) -> Vec<String> {
    let mut extra_env = daemon_env;
    extra_env.extend(env_fwd.env.iter().map(|e| format!("{}={}", e.key, e.value)));
    extra_env.extend(remote_env.iter().cloned());
    if managed_agent {
        extra_env.extend(agent_env_vars());
    }
    extra_env
}

/// Remove from `extra_env` any `KEY=…` entries whose key appears in
/// `raw_remote_env` with a value that contains a `${containerEnv:…}` token.
///
/// Phase-1 substitution collapses `${containerEnv:VAR}` to `""` because the
/// container doesn't exist yet.  If those phase-1 values are baked into the
/// compose service `environment:` block, the `userEnvProbe` reads corrupted
/// values (e.g. `PATH=:/opt/bin` instead of the real image PATH).  The
/// second-pass resolution in `build_lifecycle_env` then expands
/// `${containerEnv:PATH}` from the corrupted probe and doubles the suffix.
///
/// Stripping only the affected entries leaves daemon/agent/forwarded env vars
/// intact — the lifecycle env from `build_lifecycle_env` supplies the
/// correctly resolved `remoteEnv` values after the probe runs.
fn strip_container_env_polluted_entries(
    extra_env: &mut Vec<String>,
    raw_remote_env: Option<&serde_json::Value>,
) {
    let Some(obj) = raw_remote_env.and_then(serde_json::Value::as_object) else {
        return;
    };

    // Collect keys whose raw value contains a ${containerEnv:...} token.
    let polluted_keys: std::collections::HashSet<&str> = obj
        .iter()
        .filter_map(|(k, v)| {
            let s = v.as_str()?;
            if s.contains("${containerEnv:") {
                Some(k.as_str())
            } else {
                None
            }
        })
        .collect();

    if polluted_keys.is_empty() {
        return;
    }

    extra_env.retain(|entry| {
        let key = entry.split_once('=').map_or(entry.as_str(), |(k, _)| k);
        !polluted_keys.contains(key)
    });
}

/// Compute the mount-input fingerprint and insert it as a label on the
/// primary service. Reconnect uses this to detect drift in settings,
/// env-forwarding, or parent-git state that `config_hash` does not cover.
fn insert_mount_input_fingerprint_label(
    labels: &mut BTreeMap<String, String>,
    settings: &cella_config::CellaConfig,
    env_fwd: &cella_env::EnvForwarding,
    workspace_root: &Path,
) {
    let fp =
        crate::mount_parity::compute_mount_input_fingerprint(settings, env_fwd, workspace_root);
    labels.insert("dev.cella.mount_input_fingerprint".to_string(), fp);
}

// ---------------------------------------------------------------------------
// Mount assembly
// ---------------------------------------------------------------------------

/// Parameters for `build_compose_mount_specs`.
struct ComposeMountParams<'a> {
    workspace_root: &'a Path,
    settings: &'a cella_config::CellaConfig,
    remote_user: &'a str,
    env_fwd: &'a cella_env::EnvForwarding,
    project: &'a ComposeProject,
    config: &'a serde_json::Value,
    resolved_features: Option<&'a cella_features::ResolvedFeatures>,
    subst_ctx: &'a cella_config::devcontainer::subst::SubstitutionContext,
    /// Agent volume mount target (e.g., `/cella`). Mounts targeting this path
    /// or any descendant are rejected to protect the managed agent.
    agent_vol_target: &'a str,
    /// Agent volume name (e.g., `cella-agent`). Volume mounts aliasing this
    /// source name are rejected regardless of their target path.
    agent_vol_name: &'a str,
    /// `(docker_path, docker_compose_path)` for the compose-config probe.
    docker_binaries: (Option<String>, Option<String>),
}

/// Build compose mount specs: tool configs, SSH/GPG forwarding, parent-git,
/// user `mounts:`, and feature `mounts:`.
///
/// Sources are appended in priority order (tool configs → env-fwd → parent-git
/// → user/feature mounts) then:
/// 1. The user's base compose config is validated for agent-volume aliasing — if
///    the primary service mounts or aliases the managed agent volume, the whole
///    `cella up` is aborted with a clear error.
/// 2. Any user/feature mount targeting the agent subtree is stripped and warned.
/// 3. Remaining candidates are deduplicated against paths already declared in
///    the base compose config so the override file never shadows user-owned volumes.
///
/// Fails the whole `cella up` if `docker compose config --format json` cannot
/// resolve the base config. Reserved-agent alias rejection and named-volume
/// collision detection require a resolved model; silently skipping them would
/// be a security hole.
async fn build_compose_mount_specs(
    p: ComposeMountParams<'_>,
) -> Result<Vec<MountSpec>, crate::CellaComposeError> {
    // Assembly order mirrors single-container `config_map::map_config`:
    //   1. User devcontainer.json `mounts:` and feature `mounts:` FIRST.
    //   2. Auto-forwarded mounts (tool-config, env-fwd, parent-git) LAST.
    //
    // With last-wins dedup, placing auto-forwarded mounts after user/feature
    // mounts gives them precedence on collision — matching single-container
    // behaviour where `build_tool_config_mount_specs` + env-forwarding appends
    // override any earlier user-declared mount at the same target.
    // See: dedup_auto_forwarded_mount_wins_over_user_mount_on_collision in
    // compose_mounts.rs tests.

    // 1. User devcontainer.json `mounts:` and feature `mounts:`.
    //
    // Delegate to `map_merged_mounts`: when features are present, that function
    // uses `container_config.mounts` (which already includes both feature and
    // user mounts after `merge_with_devcontainer`); otherwise it falls back to
    // `map_additional_mounts` on the raw config.
    let substituted_fc = p.resolved_features.map(|rf| {
        cella_config::config_map::substitute_feature_config(
            rf.container_config.clone(),
            p.subst_ctx,
        )
    });
    let user_feature_mounts =
        crate::mount_parity::map_merged_mounts(p.config, substituted_fc.as_ref());
    let mut user_feature_specs = crate::mount_parity::mount_configs_to_specs(&user_feature_mounts);
    // Absolutize relative bind sources before emission. Docker Compose resolves
    // relative paths relative to the compose file's parent directory, but cella
    // writes its override to ~/.cella/…, so relative sources must be resolved
    // against the user's workspace root to point at the intended host path.
    crate::mount_parity::resolve_bind_sources(&mut user_feature_specs, p.workspace_root);
    let mut specs = user_feature_specs;

    // 2. Auto-forwarded mounts — appended last so last-wins dedup gives them
    //    precedence over a user/feature mount at the same target.
    specs.extend(cella_tool_install::build_tool_config_mount_specs(
        p.settings,
        p.remote_user,
    ));
    specs.extend(crate::mount_parity::env_fwd_to_mount_specs(p.env_fwd));

    // Parent git dir — canonicalize mirrors single-container up.rs:826-830 to
    // handle linked git worktrees whose .gitdir pointer is non-canonical.
    if let Some(parent_git) = cella_git::parent_git_dir(p.workspace_root) {
        let canonical = parent_git
            .canonicalize()
            .unwrap_or_else(|_| parent_git.clone());
        let path_str = canonical.to_string_lossy().to_string();
        specs.push(MountSpec::bind(path_str.clone(), path_str));
    }

    // Strip any mount that would shadow or alias the reserved agent volume:
    // 1. Target inside the agent subtree (e.g., /cella or /cella/bin).
    // 2. Volume mount sourcing the agent volume by name (bypasses target check).
    // Tool-config / env-fwd / parent-git mounts should never trigger these, but
    // user and feature mounts are untrusted input — apply the filter to all.
    if !p.agent_vol_target.is_empty() && !p.agent_vol_name.is_empty() {
        specs =
            crate::mount_parity::filter_reserved_agent(specs, p.agent_vol_target, p.agent_vol_name);
    }

    // Validate the base compose config and dedup candidates against it.
    //
    // Use `without_override` so that cella's own injected mounts (written in
    // step 8 above) are excluded from the resolved config.  If we used the
    // override-inclusive command the agent volume entry cella injected would
    // trigger a false-positive self-rejection on the very check designed to
    // protect that volume.
    //
    // If `docker compose config` fails, emit a warning and skip both
    // validation and dedup — Docker Compose will surface any eventual collision.
    let (dp, dcp) = p.docker_binaries;
    let validation_cmd = ComposeCommand::without_override(p.project).with_docker_binaries(dp, dcp);
    match validation_cmd.config().await {
        Ok(resolved) => {
            // Reject the whole `cella up` if the user's base compose file aliases
            // or mounts the managed agent volume. Docker Compose multi-file merge
            // appends entries — cella cannot remove base service volumes, only add.
            if !p.agent_vol_target.is_empty() && !p.agent_vol_name.is_empty() {
                crate::mount_parity::validate_base_compose_against_reserved_agent(
                    &resolved,
                    p.agent_vol_name,
                    p.agent_vol_target,
                    &p.project.primary_service,
                    p.project.run_services.as_deref(),
                )
                .map_err(|message| crate::CellaComposeError::Config { message })?;
            }
            // Dedup first: remove mounts whose target is already covered by the
            // base service.  Only the surviving (emittable) specs are then
            // validated for named-volume identity collisions.  Running the
            // collision check on the pre-dedup list would produce false positives
            // for mounts that dedup will silently drop anyway.
            let deduped = crate::mount_parity::dedup_against_base(
                &resolved,
                &p.project.primary_service,
                specs,
            )
            .map_err(|message| crate::CellaComposeError::Config { message })?;
            // Reject any extra named-volume source that collides with a base
            // top-level volume key bound to a different backing volume.  Compose
            // deep-merges top-level volume declarations, so our `name:` pin could
            // silently repoint an existing volume and break other services.
            crate::mount_parity::validate_extra_named_volumes_against_base(
                &resolved,
                &deduped,
                p.project.run_services.as_deref(),
            )
            .map_err(|message| crate::CellaComposeError::Config { message })?;
            Ok(deduped)
        }
        Err(e) => Err(crate::CellaComposeError::Config {
            message: format!(
                "cannot resolve compose config for mount validation: {e}. \
                 Cella cannot safely emit compose mounts without validating the \
                 base compose file. Fix the compose file or pin a compatible \
                 Docker Compose version."
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find a compose container by project and service name.
async fn find_compose_container(
    client: &dyn ContainerBackend,
    project_name: &str,
    service_name: &str,
) -> Result<Option<ContainerInfo>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(client
        .find_compose_service(project_name, service_name)
        .await?)
}

fn build_lifecycle_ctx<'a>(
    client: &'a dyn ContainerBackend,
    container_id: &'a str,
    user: &'a str,
    env: &'a [String],
    working_dir: Option<&'a str>,
    progress: &ProgressSender,
    masker: cella_backend::SecretMasker,
) -> LifecycleContext<'a> {
    let p = progress.clone();
    LifecycleContext {
        client,
        container_id,
        user: Some(user),
        env,
        working_dir,
        is_text: true,
        on_output: Some(Box::new(move |line| p.println(line))),
        secret_masker: masker,
    }
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

/// Build proxy forwarding config from merged cella settings.
pub fn build_proxy_forwarding_config(
    resolved: &ResolvedConfig,
    managed_agent: bool,
    skip_rules: bool,
) -> Option<cella_env::ProxyForwardingConfig> {
    let settings = cella_config::CellaConfig::load(&resolved.workspace_root, Some(resolved))
        .unwrap_or_default();
    let net_config = settings.network.to_network_config();
    let has_rules = net_config.has_rules() && !skip_rules;

    Some(cella_env::ProxyForwardingConfig {
        proxy: net_config.proxy.clone(),
        has_blocking_rules: has_rules && managed_agent,
        full_config: if has_rules && managed_agent {
            Some(net_config)
        } else {
            None
        },
        container_distro: cella_env::ca_bundle::ContainerDistro::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_extra_env_injects_browser_when_managed_agent() {
        let env_fwd = cella_env::EnvForwarding::default();
        let extra = build_extra_env(vec![], &env_fwd, &[], true);
        assert!(
            extra
                .iter()
                .any(|v| v == "BROWSER=/cella/bin/cella-browser"),
            "managed_agent=true must inject BROWSER; got {extra:?}"
        );
        assert!(
            extra.iter().any(|v| v.starts_with("CELLA_AGENT_VERSION=")),
            "managed_agent=true must inject CELLA_AGENT_VERSION; got {extra:?}"
        );
    }

    #[test]
    fn build_extra_env_omits_browser_when_not_managed() {
        let env_fwd = cella_env::EnvForwarding::default();
        let extra = build_extra_env(vec![], &env_fwd, &[], false);
        assert!(
            !extra.iter().any(|v| v.starts_with("BROWSER=")),
            "managed_agent=false must NOT inject BROWSER; got {extra:?}"
        );
    }

    #[test]
    fn build_compose_labels_stamps_cella_version() {
        // Parity with up.rs:686. Doctor's version-skew check falls back to
        // `dev.cella.version` when the live agent can't be reached; missing
        // label makes the fallback show "container unknown != CLI <ver>".
        // The label is best-effort (can go stale on CLI upgrade) — its
        // absence is handled gracefully but its presence gives users useful
        // info when the daemon is down.
        let config = serde_json::json!({});
        let config_path = PathBuf::from("/tmp/devcontainer.json");
        let workspace_root = PathBuf::from("/tmp/workspace");
        let resolved = ResolvedConfig {
            config: config.clone(),
            config_path: config_path.clone(),
            workspace_root: workspace_root.clone(),
            config_hash: String::new(),
            devcontainer_id: String::new(),
            warnings: vec![],
            typed: None,
            raw_remote_env: None,
        };
        let cfg = ComposeUpConfig {
            resolved: &resolved,
            config: &config,
            config_path: &config_path,
            workspace_root: &workspace_root,
            container_name: "test-container",
            remote_env: &[],
            cli_remote_env: &[],
            resolution: ContainerResolution::default(),
            skip_checksum: false,
            profiles: vec![],
            env_files: vec![],
            pull_policy: None,
            network_rule_policy: cella_network::NetworkRulePolicy::Enforce,
            user_env_probe: cella_env::user_env_probe::UserEnvProbe::default(),
            lifecycle_gate: cella_backend::LifecycleGate::default(),
            build_tuning: ComposeBuildTuning::default(),
            gpu_availability: cella_backend::GpuAvailability::default(),
            update_remote_user_uid_default: cella_backend::UpdateRemoteUserUidDefault::default(),
            omit_remote_env_from_metadata: false,
            lockfile_policy: cella_features::LockfilePolicy::default(),
            dotfiles: DotfilesConfig::default(),
        };
        let project = ComposeProject {
            project_name: "cella-test".to_string(),
            compose_files: vec![],
            override_file: PathBuf::from("/tmp/override.yaml"),
            primary_service: "app".to_string(),
            run_services: None,
            shutdown_action: crate::ShutdownAction::StopCompose,
            override_command: false,
            workspace_folder: "/workspace".to_string(),
            config_dir: PathBuf::from("/tmp"),
            workspace_root: workspace_root.clone(),
            config_hash: "hash123".to_string(),
            profiles: vec![],
            env_files: vec![],
            pull_policy: None,
        };

        let labels = build_compose_labels(&cfg, &project, "vscode");
        let version = labels
            .get("dev.cella.version")
            .expect("compose labels must include dev.cella.version");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            labels.get("dev.cella.user_env_probe").map(String::as_str),
            Some("loginInteractiveShell")
        );
    }

    #[test]
    fn build_compose_labels_include_runtime_project_and_spec_identity() {
        let config = serde_json::json!({});
        let config_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("devcontainer.json");
        std::fs::write(&config_path, "{}").unwrap();
        let resolved = ResolvedConfig {
            config: config.clone(),
            config_path: config_path.clone(),
            workspace_root: workspace_dir.path().to_path_buf(),
            config_hash: String::new(),
            devcontainer_id: String::new(),
            warnings: vec![],
            typed: None,
            raw_remote_env: None,
        };
        let cfg = ComposeUpConfig {
            resolved: &resolved,
            config: &config,
            config_path: &config_path,
            workspace_root: workspace_dir.path(),
            container_name: "test-container",
            remote_env: &[],
            cli_remote_env: &[],
            resolution: ContainerResolution::default(),
            skip_checksum: false,
            profiles: vec![],
            env_files: vec![],
            pull_policy: None,
            network_rule_policy: cella_network::NetworkRulePolicy::Enforce,
            user_env_probe: cella_env::user_env_probe::UserEnvProbe::default(),
            lifecycle_gate: cella_backend::LifecycleGate::default(),
            build_tuning: ComposeBuildTuning::default(),
            gpu_availability: cella_backend::GpuAvailability::default(),
            update_remote_user_uid_default: cella_backend::UpdateRemoteUserUidDefault::default(),
            omit_remote_env_from_metadata: false,
            lockfile_policy: cella_features::LockfilePolicy::default(),
            dotfiles: DotfilesConfig::default(),
        };
        let project = ComposeProject {
            project_name: "cella-test-project".to_string(),
            compose_files: vec![config_dir.path().join("docker-compose.yml")],
            override_file: config_dir.path().join("docker-compose.cella.yml"),
            primary_service: "web".to_string(),
            run_services: Some(vec!["web".to_string(), "worker".to_string()]),
            shutdown_action: crate::ShutdownAction::StopCompose,
            override_command: false,
            workspace_folder: "/workspace/project".to_string(),
            config_dir: config_dir.path().to_path_buf(),
            workspace_root: workspace_dir.path().to_path_buf(),
            config_hash: "hash456".to_string(),
            profiles: vec!["dev".to_string()],
            env_files: vec![config_dir.path().join(".env")],
            pull_policy: Some("missing".to_string()),
        };

        let labels = build_compose_labels(&cfg, &project, "node");

        assert_eq!(
            labels.get("dev.cella.compose_project").map(String::as_str),
            Some("cella-test-project")
        );
        assert_eq!(
            labels.get("dev.cella.primary_service").map(String::as_str),
            Some("web")
        );
        assert_eq!(
            labels.get("dev.cella.workspace_folder").map(String::as_str),
            Some("/workspace/project")
        );
        assert_eq!(
            labels.get("dev.cella.remote_user").map(String::as_str),
            Some("node")
        );
        assert_eq!(
            labels.get("devcontainer.local_folder"),
            labels.get("dev.cella.workspace_path")
        );
        assert_eq!(
            labels.get("devcontainer.config_file"),
            labels.get("dev.cella.config_path")
        );
        assert!(labels.contains_key("dev.cella.docker_runtime"));
    }

    #[test]
    fn compose_metadata_label_features_reuses_precomputed_label() {
        // Features path: the label computed during feature resolution (base +
        // features + config) is reused verbatim; base/config args are ignored.
        let precomputed = r#"[{"id":"ghcr.io/x/y:1"},{"remoteUser":"vscode"}]"#;
        let config = serde_json::json!({ "remoteUser": "node" });
        let label = compose_metadata_label(
            Some(precomputed),
            &config,
            Some(r#"[{"id":"base"}]"#),
            false,
        );
        assert_eq!(label, precomputed);
    }

    #[test]
    fn compose_metadata_label_no_features_merges_base_then_config() {
        // No features: base image metadata entries are prepended and the config
        // entry appended -- matching official getDevcontainerMetadata.
        let base = r#"[{"id":"ghcr.io/base/feat:1"}]"#;
        let config = serde_json::json!({ "remoteUser": "vscode" });
        let label = compose_metadata_label(None, &config, Some(base), false);
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(&label).expect("metadata label is a JSON array");
        assert_eq!(parsed.len(), 2, "base entry + config entry: {label}");
        assert_eq!(parsed[0]["id"], "ghcr.io/base/feat:1");
        assert_eq!(parsed[1], config);
    }

    #[test]
    fn compose_metadata_label_no_features_no_base_is_config_only() {
        let config = serde_json::json!({ "remoteUser": "vscode" });
        let label = compose_metadata_label(None, &config, None, false);
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(&label).expect("metadata label is a JSON array");
        assert_eq!(parsed, vec![config]);
    }

    #[test]
    fn insert_mount_input_fingerprint_label_changes_with_forwarded_mounts() {
        let workspace = tempfile::tempdir().unwrap();
        let settings = cella_config::CellaConfig::default();
        let mut labels = BTreeMap::new();
        let env_fwd = cella_env::EnvForwarding::default();

        insert_mount_input_fingerprint_label(&mut labels, &settings, &env_fwd, workspace.path());
        let base = labels
            .get("dev.cella.mount_input_fingerprint")
            .cloned()
            .expect("fingerprint label");

        let mut changed_env_fwd = cella_env::EnvForwarding::default();
        changed_env_fwd.mounts.push(cella_env::ForwardMount {
            source: workspace.path().join("sock").to_string_lossy().into_owned(),
            target: "/tmp/socket".to_string(),
        });
        insert_mount_input_fingerprint_label(
            &mut labels,
            &settings,
            &changed_env_fwd,
            workspace.path(),
        );
        let changed = labels
            .get("dev.cella.mount_input_fingerprint")
            .expect("updated fingerprint label");

        assert_ne!(&base, changed);
    }

    /// Test that `lifecycle_secret_masker` from the trait default is passthrough,
    /// and that a non-empty masker built from `SecretMasker::new` actually redacts.
    ///
    /// `build_lifecycle_ctx` is not called directly here because it requires a
    /// `&dyn ContainerBackend` — no test double exists in this crate. The CLI
    /// override path (`CliComposeUpHooks::lifecycle_secret_masker`) is covered
    /// transitively by `cella_backend::SecretMasker`'s own unit tests. This test
    /// verifies the plumbing at the trait boundary: default → passthrough, and
    /// the non-default value produced by `SecretMasker::new` actually masks.
    #[test]
    fn lifecycle_secret_masker_trait_default_is_passthrough() {
        use std::future::Future;
        use std::pin::Pin;

        struct NoopHooks;

        impl ComposeUpHooks for NoopHooks {
            fn daemon_env<'a>(
                &'a self,
                _container_name: &'a str,
                _host_gateway: &'a str,
            ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
                Box::pin(async { vec![] })
            }

            fn sync_agent_runtime<'a>(
                &'a self,
                _client: &'a dyn ContainerBackend,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
                Box::pin(async {})
            }

            fn register_container<'a>(
                &'a self,
                _client: &'a dyn ContainerBackend,
                _container_id: &'a str,
                _config: &'a serde_json::Value,
                _container_name: &'a str,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
                Box::pin(async {})
            }

            fn launch_agent<'a>(
                &'a self,
                _client: &'a dyn ContainerBackend,
                _container_id: &'a str,
                _agent_arch: &'a str,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
                Box::pin(async {})
            }

            fn post_create_setup<'a>(
                &'a self,
                _client: &'a dyn ContainerBackend,
                _container_id: &'a str,
                _remote_user: &'a str,
                _config: &'a serde_json::Value,
                _workspace_root: &'a Path,
                _remote_env: &'a [String],
            ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
                Box::pin(async { vec![] })
            }
        }

        let hooks = NoopHooks;

        // Default impl returns an empty (passthrough) masker.
        let default_masker = hooks.lifecycle_secret_masker();
        assert!(
            default_masker.is_empty(),
            "default lifecycle_secret_masker must be empty (passthrough)"
        );
        let s = "token=s3cr3tvalue";
        assert_eq!(
            default_masker.mask(s),
            s,
            "empty masker must not modify the input"
        );

        // A masker built from real secret entries does redact.
        let real_masker = cella_backend::SecretMasker::new(&["SECRET=s3cr3tvalue".to_string()]);
        assert!(
            !real_masker
                .mask("token=s3cr3tvalue")
                .contains("s3cr3tvalue"),
            "non-empty masker must redact the secret value"
        );
    }

    #[test]
    fn build_extra_env_preserves_precedence_order() {
        // daemon_env first, then env_fwd, then remote_env, then agent vars.
        let env_fwd = cella_env::EnvForwarding {
            env: vec![cella_env::ForwardEnv {
                key: "FWD_KEY".into(),
                value: "fwd_val".into(),
            }],
            ..Default::default()
        };
        let extra = build_extra_env(
            vec!["CELLA_DAEMON_ADDR=h:1".to_string()],
            &env_fwd,
            &["USER_KEY=user_val".to_string()],
            true,
        );
        let daemon_idx = extra
            .iter()
            .position(|v| v == "CELLA_DAEMON_ADDR=h:1")
            .unwrap();
        let fwd_idx = extra.iter().position(|v| v == "FWD_KEY=fwd_val").unwrap();
        let user_idx = extra.iter().position(|v| v == "USER_KEY=user_val").unwrap();
        let browser_idx = extra
            .iter()
            .position(|v| v == "BROWSER=/cella/bin/cella-browser")
            .unwrap();
        assert!(daemon_idx < fwd_idx);
        assert!(fwd_idx < user_idx);
        assert!(user_idx < browser_idx);
    }

    // ── strip_container_env_polluted_entries ──────────────────────────────────

    /// Keys with `${containerEnv:…}` in their raw value are stripped from
    /// `extra_env` so the `userEnvProbe` reads the real image env instead of the
    /// phase-1 collapsed (empty) value.
    #[test]
    fn strip_removes_container_env_token_keys() {
        let raw = serde_json::json!({
            "PATH": "${containerEnv:PATH}:/opt/bin",
            "PLAIN": "no_token"
        });
        let mut extra = vec![
            "PATH=:/opt/bin".to_string(), // phase-1 collapsed value
            "PLAIN=no_token".to_string(),
            "OTHER=untouched".to_string(),
        ];
        strip_container_env_polluted_entries(&mut extra, Some(&raw));
        // PATH had a token → stripped; PLAIN and OTHER are clean → preserved.
        assert!(
            !extra.iter().any(|e| e.starts_with("PATH=")),
            "PATH with ${{containerEnv:…}} must be stripped; got {extra:?}"
        );
        assert!(
            extra.contains(&"PLAIN=no_token".to_string()),
            "PLAIN without token must be preserved; got {extra:?}"
        );
        assert!(
            extra.contains(&"OTHER=untouched".to_string()),
            "non-remoteEnv entries must be preserved; got {extra:?}"
        );
    }

    /// When `raw_remote_env` is absent, the function is a no-op.
    #[test]
    fn strip_no_op_when_raw_remote_env_absent() {
        let mut extra = vec!["PATH=/usr/bin".to_string(), "FOO=bar".to_string()];
        let before = extra.clone();
        strip_container_env_polluted_entries(&mut extra, None);
        assert_eq!(extra, before, "no-op when raw_remote_env is absent");
    }

    /// When `raw_remote_env` has no `${containerEnv:…}` tokens, nothing is
    /// stripped (additive invariant: common case unaffected).
    #[test]
    fn strip_no_op_when_no_container_env_tokens() {
        let raw = serde_json::json!({"MYVAR": "static_value"});
        let mut extra = vec!["MYVAR=static_value".to_string(), "OTHER=x".to_string()];
        let before = extra.clone();
        strip_container_env_polluted_entries(&mut extra, Some(&raw));
        assert_eq!(
            extra, before,
            "no-op when no ${{containerEnv:…}} tokens present"
        );
    }
}
