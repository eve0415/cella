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

use cella_backend::{
    ContainerBackend, ContainerInfo, ContainerState, LifecycleContext, MountSpec,
    run_lifecycle_phase,
};
use cella_compose::{ComposeCommand, ComposeProject, OverrideConfig, ServiceBuildInfo};

use crate::container_setup::{resolve_remote_user, run_host_command, verify_container_running};
use crate::lifecycle::{lifecycle_entries_for_phase, run_lifecycle_entries};
use crate::progress::ProgressSender;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a compose up invocation.
pub struct ComposeUpConfig<'a> {
    /// Parsed devcontainer JSON.
    pub config: &'a serde_json::Value,
    /// Path to devcontainer.json.
    pub config_path: &'a Path,
    /// Workspace root on the host.
    pub workspace_root: &'a Path,
    /// Container name for daemon registration.
    pub container_name: &'a str,
    /// Extra environment variables to inject (`KEY=VALUE` format).
    pub remote_env: &'a [String],
    /// Whether to tear down and recreate existing containers.
    pub remove_container: bool,
    /// Whether to rebuild with `--no-cache`.
    pub build_no_cache: bool,
    /// Skip agent checksum verification.
    pub skip_checksum: bool,
    /// Docker Compose profiles to activate (`--profile` flags).
    pub profiles: Vec<String>,
    /// Extra env-file paths for docker compose (`--env-file` flags).
    pub env_files: Vec<PathBuf>,
    /// Pull policy for docker compose up/build (`--pull` flag).
    pub pull_policy: Option<String>,
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
    /// Whether the container was freshly created or already running.
    pub outcome: ComposeUpOutcome,
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

/// Callbacks for operations that live outside the orchestrator's dependency
/// graph (daemon management, agent launch, etc.).
pub trait ComposeUpHooks: Send + Sync {
    /// Ensure the daemon is running and return env vars to inject.
    fn daemon_env<'a>(
        &'a self,
        container_name: &'a str,
        host_gateway: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>;

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
        cella_compose::parse::validate_primary_service(
            &project.compose_files,
            &project.primary_service,
        )?;
        if let Some(ref run_services) = project.run_services {
            cella_compose::parse::validate_run_services(&project.compose_files, run_services)?;
        }
        Ok::<(), cella_compose::CellaComposeError>(())
    })
    .await?;

    // 3. Run initializeCommand on host (runs every invocation per spec)
    if let Some(init_cmd) = config.get("initializeCommand") {
        run_host_command("initializeCommand", init_cmd)?;
    }

    // 4. Check for existing compose project
    let existing =
        find_compose_container(client, &project.project_name, &project.primary_service).await?;

    if let Some(ref container) = existing {
        if container.state == ContainerState::Running
            && !cfg.remove_container
            && !cfg.build_no_cache
        {
            info!("Compose project already running, running postAttachCommand only");
            return handle_compose_running(&ctx, &project, container).await;
        }

        if cfg.remove_container || cfg.build_no_cache {
            run_step_result(&progress, "Stopping existing compose project...", async {
                let compose_cmd = ComposeCommand::from_project_name(&project.project_name);
                compose_cmd.down().await
            })
            .await?;
        }
    }

    // 5-13. Prepare environment, write override, start services
    let (remote_user, resolved_features, agent_arch) = prepare_and_start(&ctx, &project).await?;

    // 14-20. Post-start: find container, setup, lifecycle, output
    finalize_compose(
        &ctx,
        &project,
        &remote_user,
        resolved_features.as_ref(),
        &agent_arch,
    )
    .await
}

// ---------------------------------------------------------------------------
// Prepare and start (steps 5-13)
// ---------------------------------------------------------------------------

/// Prepare environment, write override YAML, and start compose services.
async fn prepare_and_start(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
) -> Result<
    (String, Option<cella_features::ResolvedFeatures>, String),
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
    cella_compose::check_compose_features_support().await?;

    // 6. Resolve features via combined-Dockerfile approach (if features configured)
    let features_build = crate::compose_features::resolve_compose_features(
        client,
        config,
        cfg.config_path,
        project,
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
        base_compose_volumes: std::collections::BTreeSet::new(),
    };
    write_build_override(project, features_build.as_ref(), &build_ov)?;

    // 9. Run docker compose build to ensure images exist for inspection.
    let compose_cmd = ComposeCommand::new(project);
    run_step_result(
        progress,
        "Building compose services...",
        compose_cmd.build(None),
    )
    .await?;

    // 10. Resolve remote user from built image metadata.
    let (image_user, image_meta_user) =
        resolve_compose_image_info(client, project, features_build.as_ref(), progress).await;
    let remote_user = resolve_remote_user(config, image_meta_user.as_ref(), &image_user);
    let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user, None);
    info!("Resolved remote user: {remote_user} (image user: {image_user})");

    // 11-12. Build extra env vars and labels for the primary service.
    let extra_env = build_extra_env(daemon_env, &env_fwd, cfg.remote_env);
    let labels = build_compose_labels(cfg, project, &remote_user);

    let settings = cella_config::settings::Settings::load(cfg.workspace_root);
    let (mount_specs, base_compose_volumes) = build_compose_mount_specs(ComposeMountParams {
        workspace_root: cfg.workspace_root,
        settings: &settings,
        remote_user: &remote_user,
        env_fwd: &env_fwd,
        compose_cmd: &compose_cmd,
        project,
        config,
        resolved_features: features_build.as_ref().map(|fb| &fb.resolved_features),
        agent_vol_target: &agent_vol_target,
        agent_vol_name: &agent_vol_name,
    })
    .await;

    let ov_ctx = OverrideContext {
        agent_vol_name,
        agent_vol_target,
        extra_env,
        labels,
        extra_volumes: mount_specs,
        base_compose_volumes,
    };

    // 13. Build-time UID remap: build a thin image layer with correct UID/GID.
    let uid_image = build_uid_remap_image_compose(
        ctx,
        project,
        features_build.as_ref(),
        &remote_user,
        &image_user,
    )
    .await?;

    // 14. Write final override with labels, env, and UID remap image.
    write_final_override(project, features_build.as_ref(), &ov_ctx, uid_image)?;

    // 15. docker compose up -d (idempotent)
    run_step_result(progress, "Starting compose services...", async {
        compose_cmd.up(project.run_services.as_deref(), false).await
    })
    .await?;

    let resolved_features = features_build.map(|b| b.resolved_features);
    Ok((remote_user, resolved_features, agent_arch))
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

    // 16. Register with daemon (primary container only)
    hooks
        .register_container(client, &primary.id, config, cfg.container_name)
        .await;

    // 17. Post-create setup (UID, env, credentials, tools, userEnvProbe)
    let lifecycle_env = hooks
        .post_create_setup(
            client,
            &primary.id,
            remote_user,
            config,
            cfg.workspace_root,
            cfg.remote_env,
        )
        .await;

    // 18. Launch agent as background process via exec
    hooks.launch_agent(client, &primary.id, agent_arch).await;

    // 19. Run lifecycle phases (primary service only)
    let metadata = resolved_features.map(|rf| rf.metadata_label.as_str());
    for phase in [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ] {
        let entries = lifecycle_entries_for_phase(metadata, config, phase);
        let lc_ctx = build_lifecycle_ctx(
            client,
            &primary.id,
            remote_user,
            &lifecycle_env,
            Some(&project.workspace_folder),
            progress,
        );
        run_lifecycle_entries(&lc_ctx, phase, &entries, progress).await?;
    }

    Ok(ComposeUpResult {
        container_id: primary.id,
        remote_user: remote_user.to_string(),
        workspace_folder: project.workspace_folder.clone(),
        outcome: ComposeUpOutcome::Created,
    })
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

    // Re-register with daemon in case it restarted
    hooks
        .register_container(client, &container.id, config, cfg.container_name)
        .await;

    if let Some(cmd) = config.get("postAttachCommand")
        && !cmd.is_null()
    {
        let lifecycle_env: Vec<String> = cfg.remote_env.to_vec();
        let lc_ctx = build_lifecycle_ctx(
            client,
            &container.id,
            &remote_user,
            &lifecycle_env,
            Some(project.workspace_folder.as_str()),
            progress,
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
        outcome: ComposeUpOutcome::Running,
    })
}

/// Compose override context shared between build and UID remap override writes.
struct OverrideContext {
    agent_vol_name: String,
    agent_vol_target: String,
    extra_env: Vec<String>,
    labels: BTreeMap<String, String>,
    extra_volumes: Vec<MountSpec>,
    /// Top-level volume names from the user's base compose config.
    /// Passed through to the override YAML generator to avoid emitting
    /// duplicate top-level declarations for user-owned volumes.
    base_compose_volumes: std::collections::BTreeSet<String>,
}

// ---------------------------------------------------------------------------
// Override helpers
// ---------------------------------------------------------------------------

/// Write the initial compose override YAML for building with features.
fn write_build_override(
    project: &ComposeProject,
    features_build: Option<&crate::compose_features::ComposeFeaturesBuild>,
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
        extra_volumes: Vec::new(),
        base_compose_volumes: std::collections::BTreeSet::new(),
    };
    let override_yaml = cella_compose::override_file::generate_override_yaml(&override_config);
    cella_compose::override_file::write_override_file(&project.override_file, &override_yaml)?;
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
/// Returns `(image_user, Option<ImageMetadataUserInfo>)`. Falls back to
/// `("root", None)` when inspection fails.
async fn resolve_compose_image_info(
    client: &dyn ContainerBackend,
    project: &ComposeProject,
    features_build: Option<&crate::compose_features::ComposeFeaturesBuild>,
    progress: &ProgressSender,
) -> (String, Option<cella_features::ImageMetadataUserInfo>) {
    // If features resolved an image, its metadata was already extracted.
    if let Some(fb) = features_build {
        let meta_user = fb
            .base_image_metadata
            .as_deref()
            .map(|m| cella_features::parse_image_metadata(m).1);
        return (fb.image_user.clone(), meta_user);
    }

    // Resolve compose config to find the service's image source.
    let compose_cmd = ComposeCommand::without_override(project);
    let resolved = match compose_cmd.config().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to resolve compose config for image metadata: {e}");
            return ("root".to_string(), None);
        }
    };

    let service_info =
        match cella_compose::extract_service_build_info(&resolved, &project.primary_service) {
            Ok(info) => info,
            Err(e) => {
                warn!("Failed to extract service build info: {e}");
                return ("root".to_string(), None);
            }
        };

    let image_name = match &service_info {
        ServiceBuildInfo::Image { image } => {
            // Pull the image if not locally available.
            if matches!(client.image_exists(image).await, Ok(false)) {
                let _ = run_step_result(
                    progress,
                    "Pulling compose service image...",
                    client.pull_image(image),
                )
                .await;
            }
            image.clone()
        }
        ServiceBuildInfo::Build { .. } => {
            // After compose build, the image exists as {project}-{service}.
            format!("{}-{}", project.project_name, project.primary_service)
        }
    };

    match client.inspect_image_details(&image_name).await {
        Ok(details) => {
            let meta_user = details
                .metadata
                .as_deref()
                .map(|m| cella_features::parse_image_metadata(m).1);
            (details.user, meta_user)
        }
        Err(e) => {
            warn!("Failed to inspect image '{image_name}' for metadata: {e}");
            ("root".to_string(), None)
        }
    }
}

// ---------------------------------------------------------------------------
// UID remap
// ---------------------------------------------------------------------------

/// Build a UID-remapped image for the compose service.
///
/// Returns the UID-remapped image name, or `None` if remap was skipped.
async fn build_uid_remap_image_compose(
    ctx: &Ctx<'_>,
    project: &ComposeProject,
    features_build: Option<&crate::compose_features::ComposeFeaturesBuild>,
    remote_user: &str,
    image_user: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let update_uid = ctx
        .cfg
        .config
        .get("updateRemoteUserUID")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    if !update_uid {
        return Ok(None);
    }

    let compose_image = features_build
        .and_then(|b| b.image_name_override.clone())
        .unwrap_or_else(|| format!("{}-{}", project.project_name, project.primary_service));

    crate::uid_image::build_uid_remap_image(
        ctx.client,
        &compose_image,
        image_user,
        remote_user,
        ctx.progress,
    )
    .await
}

/// Write the final compose override with labels, env, and optional UID remap image.
fn write_final_override(
    project: &ComposeProject,
    features_build: Option<&crate::compose_features::ComposeFeaturesBuild>,
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
        extra_volumes: ov.extra_volumes.clone(),
        base_compose_volumes: ov.base_compose_volumes.clone(),
    };
    let override_yaml = cella_compose::override_file::generate_override_yaml(&override_config);
    cella_compose::override_file::write_override_file(&project.override_file, &override_yaml)?;
    debug!(
        "Final override written to: {}",
        project.override_file.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

/// Build cella labels for the compose override file.
///
/// Includes both cella-specific and spec-standard labels for VS Code interop.
fn build_compose_labels(
    cfg: &ComposeUpConfig<'_>,
    project: &ComposeProject,
    remote_user: &str,
) -> BTreeMap<String, String> {
    let workspace_str = cfg
        .workspace_root
        .canonicalize()
        .unwrap_or_else(|_| cfg.workspace_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    let config_str = cfg
        .config_path
        .canonicalize()
        .unwrap_or_else(|_| cfg.config_path.to_path_buf())
        .to_string_lossy()
        .to_string();

    let mut labels = BTreeMap::new();

    // Cella-specific labels.
    labels.insert("dev.cella.tool".to_string(), "cella".to_string());
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

    // Spec-standard labels for VS Code / tooling interop.
    labels.insert("devcontainer.local_folder".to_string(), workspace_str);
    labels.insert("devcontainer.config_file".to_string(), config_str);

    labels
}

/// Assemble the extra environment variable list for the compose service.
///
/// Combines daemon-injected env vars, forwarded env vars (SSH/GPG agent sockets,
/// etc.), and user-specified `remote_env` overrides in precedence order.
fn build_extra_env(
    daemon_env: Vec<String>,
    env_fwd: &cella_env::EnvForwarding,
    remote_env: &[String],
) -> Vec<String> {
    let mut extra_env = daemon_env;
    extra_env.extend(env_fwd.env.iter().map(|e| format!("{}={}", e.key, e.value)));
    extra_env.extend(remote_env.iter().cloned());
    extra_env
}

// ---------------------------------------------------------------------------
// Mount assembly
// ---------------------------------------------------------------------------

/// Parameters for `build_compose_mount_specs`.
struct ComposeMountParams<'a> {
    workspace_root: &'a Path,
    settings: &'a cella_config::settings::Settings,
    remote_user: &'a str,
    env_fwd: &'a cella_env::EnvForwarding,
    compose_cmd: &'a ComposeCommand,
    project: &'a ComposeProject,
    config: &'a serde_json::Value,
    resolved_features: Option<&'a cella_features::ResolvedFeatures>,
    /// Agent volume mount target (e.g., `/cella`). Mounts targeting this path
    /// or any descendant are rejected to protect the managed agent.
    agent_vol_target: &'a str,
    /// Agent volume name (e.g., `cella-agent`). Volume mounts aliasing this
    /// source name are rejected regardless of their target path.
    agent_vol_name: &'a str,
}

/// Build compose mount specs: tool configs, SSH/GPG forwarding, parent-git,
/// user `mounts:`, and feature `mounts:`.
///
/// Sources are appended in priority order (tool configs → env-fwd → parent-git
/// → user/feature mounts) then:
/// 1. Any user/feature mount targeting the agent subtree is stripped and warned.
/// 2. Remaining candidates are deduplicated against paths already declared in
///    the base compose config so the override file never shadows user-owned volumes.
///
/// Returns `(mount_specs, base_compose_volumes)` where `base_compose_volumes` is
/// the set of top-level volume names from the user's base compose config. This is
/// needed by the override YAML generator to avoid emitting duplicate declarations
/// for volumes the user already declared.
async fn build_compose_mount_specs(
    p: ComposeMountParams<'_>,
) -> (Vec<MountSpec>, std::collections::BTreeSet<String>) {
    let mut specs = crate::tool_install::build_tool_config_mount_specs(p.settings, p.remote_user);
    specs.extend(crate::compose_mounts::env_fwd_to_mount_specs(p.env_fwd));

    // Parent git dir — canonicalize mirrors single-container up.rs:826-830 to
    // handle linked git worktrees whose .gitdir pointer is non-canonical.
    if let Some(parent_git) = cella_git::parent_git_dir(p.workspace_root) {
        let canonical = parent_git
            .canonicalize()
            .unwrap_or_else(|_| parent_git.clone());
        let path_str = canonical.to_string_lossy().to_string();
        specs.push(MountSpec::bind(path_str.clone(), path_str));
    }

    // User devcontainer.json `mounts:` and feature `mounts:`.
    //
    // Delegate to `map_merged_mounts`: when features are present, that function
    // uses `container_config.mounts` (which already includes both feature and
    // user mounts after `merge_with_devcontainer`); otherwise it falls back to
    // `map_additional_mounts` on the raw config.
    let feature_config = p.resolved_features.map(|rf| &rf.container_config);
    let user_feature_mounts = crate::config_map::map_merged_mounts(p.config, feature_config);
    specs.extend(crate::compose_mounts::mount_configs_to_specs(
        &user_feature_mounts,
    ));

    // Strip any mount that would shadow or alias the reserved agent volume:
    // 1. Target inside the agent subtree (e.g., /cella or /cella/bin).
    // 2. Volume mount sourcing the agent volume by name (bypasses target check).
    // Tool-config / env-fwd / parent-git mounts should never trigger these, but
    // user and feature mounts are untrusted input — apply the filter to all.
    if !p.agent_vol_target.is_empty() && !p.agent_vol_name.is_empty() {
        specs = crate::compose_mounts::filter_reserved_agent(
            specs,
            p.agent_vol_target,
            p.agent_vol_name,
        );
    }

    // Dedup against the user's base compose config. If this call fails, emit a
    // warning and skip dedup — Docker Compose will surface any eventual collision.
    // Also extract the top-level volume names from the resolved config so the
    // override generator can avoid duplicating user-declared volumes.
    match p.compose_cmd.config().await {
        Ok(resolved) => {
            let base_vols = resolved.volumes.keys().cloned().collect();
            let deduped = crate::compose_mounts::dedup_against_base(
                &resolved,
                &p.project.primary_service,
                specs,
            );
            (deduped, base_vols)
        }
        Err(e) => {
            warn!("compose config unavailable for mount dedup ({e}); emitting all candidates");
            (specs, std::collections::BTreeSet::new())
        }
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
