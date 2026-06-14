//! Docker Compose build pipeline.
//!
//! Resolves features, writes the compose override file, and runs
//! `docker compose build`. Shared by both `cella build` and `cella up`
//! (compose path).

use std::path::{Path, PathBuf};

use cella_backend::progress::ProgressSender;
use cella_backend::{BuildSecret, ContainerBackend};

use crate::ComposeProject;

/// Result of a compose build operation.
pub struct ComposeBuildResult {
    /// The primary service's image name. When features are resolved this is the
    /// combined image cella builds; otherwise it is the service's own image
    /// (`image:` reference, or `{project}-{service}` for a build-based service).
    pub image_name: String,
    /// Whether features were resolved and a combined Dockerfile was generated.
    pub had_features: bool,
}

/// Configuration for a compose build invocation.
pub struct ComposeBuildConfig<'a> {
    /// Parsed devcontainer JSON.
    pub config: &'a serde_json::Value,
    /// Path to devcontainer.json.
    pub config_path: &'a Path,
    /// Workspace root on the host.
    pub workspace_root: &'a Path,
    /// Docker Compose profiles to activate (`--profile` flags).
    pub profiles: Vec<String>,
    /// Extra env-file paths for docker compose (`--env-file` flags).
    pub env_files: Vec<PathBuf>,
    /// Pull policy for docker compose build (`--pull` flag).
    pub pull_policy: Option<String>,
    /// `BuildKit` secrets forwarded to compose builds.
    pub secrets: Vec<BuildSecret>,
    /// `docker` CLI binary path (`--docker-path`). `None` = `docker`.
    pub docker_path: Option<String>,
    /// Standalone `docker-compose` (V1) binary (`--docker-compose-path`).
    /// Stored and forwarded; cella is V2-only so it is currently unused.
    pub docker_compose_path: Option<String>,
    /// Feature lockfile policy derived from `--no-lockfile` / `--frozen-lockfile`.
    pub lockfile_policy: cella_features::LockfilePolicy,
}

/// Run the compose build pipeline: resolve features, write override, build.
///
/// # Errors
///
/// Returns an error if feature resolution, override generation, or the
/// compose build command fails.
pub async fn compose_build(
    client: &dyn ContainerBackend,
    cfg: &ComposeBuildConfig<'_>,
    progress: &ProgressSender,
) -> Result<ComposeBuildResult, Box<dyn std::error::Error + Send + Sync>> {
    if !client.capabilities().compose {
        return Err(format!(
            "selected backend '{}' does not support Docker Compose devcontainers",
            client.kind()
        )
        .into());
    }

    let config = cfg.config;
    let config_path = cfg.config_path;
    let workspace_root = cfg.workspace_root;
    let mut project = ComposeProject::from_resolved(config, config_path, workspace_root)?;
    project.set_compose_options(
        cfg.profiles.clone(),
        cfg.env_files.clone(),
        cfg.pull_policy.clone(),
    );

    // Resolve features via combined-Dockerfile approach
    let features_build = crate::combined_dockerfile_build::resolve_compose_features(
        client,
        config,
        config_path,
        &project,
        // `cella build` does not yet expose --omit-config-remote-env-from-metadata
        // (it's wired on the `up` path only); keep the full metadata label here.
        false,
        cfg.lockfile_policy,
        progress,
    )
    .await?;

    // Write override file if features were resolved
    if let Some(ref fb) = features_build {
        let (agent_vol_name, agent_vol_target, _) = if client.capabilities().managed_agent {
            client.agent_volume_mount()
        } else {
            (String::new(), String::new(), true)
        };
        let compose_secrets: Vec<crate::ComposeSecret> = cfg
            .secrets
            .iter()
            .map(|s| crate::ComposeSecret {
                id: s.id.clone(),
                file: s.src.clone(),
                environment: s.env.clone(),
            })
            .collect();
        let override_config = crate::OverrideConfig {
            primary_service: project.primary_service.clone(),
            image_override: fb.image_name_override.clone(),
            override_command: project.override_command,
            agent_volume_name: agent_vol_name,
            agent_volume_target: agent_vol_target,
            extra_env: Vec::new(),
            extra_labels: std::collections::BTreeMap::new(),
            build_dockerfile: Some(fb.combined_dockerfile.clone()),
            build_target: Some(fb.build_target.clone()),
            build_context: fb.build_context.clone(),
            additional_contexts: fb.additional_contexts.clone(),
            build_secrets: compose_secrets,
            extra_volumes: Vec::new(),
            // GPU reservation is emitted only in the final override.
            request_gpu: false,
            // `cella build` only builds the image; runtime security props apply at `up`.
            security: cella_config::config_map::MergedSecurityConfig::default(),
        };
        let yaml = crate::override_file::generate_override_yaml(&override_config);
        crate::override_file::write_override_file(&project.override_file, &yaml)?;
    }

    // Run docker compose build. The override file is only written when features
    // are resolved (the `if let Some(ref fb)` block above); on the no-features
    // path it does not exist, so use a command without it — otherwise
    // `docker compose -f <missing-override> build` fails with "no such file".
    let compose_cmd = if features_build.is_some() {
        crate::ComposeCommand::new(&project)
    } else {
        crate::ComposeCommand::without_override(&project)
    }
    .with_docker_binaries(cfg.docker_path.clone(), cfg.docker_compose_path.clone());
    compose_cmd.build(None, false).await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("docker compose build failed: {e}").into()
        },
    )?;

    let had_features = features_build.is_some();
    let image_name = if let Some(image) = features_build.and_then(|b| b.image_name_override) {
        image
    } else {
        // No features: resolve the service's real image. `compose_cmd` was built
        // without the override above, so it is safe for `docker compose config`.
        resolve_primary_service_image(&compose_cmd, &project).await?
    };

    Ok(ComposeBuildResult {
        image_name,
        had_features,
    })
}

/// Resolve the primary service's image name from the resolved compose config.
///
/// Runs `docker compose config` through `compose_cmd` and maps the primary
/// service's build/image info to the name it resolves to after a build
/// (`image:` reference, or `{project}-{service}` for a build-based service).
/// `compose_cmd` must be built without the override file, since this runs on the
/// no-features path where that file is never written.
///
/// # Errors
///
/// Returns an error if `docker compose config` fails or the primary service has
/// neither `build` nor `image`.
pub(crate) async fn resolve_primary_service_image(
    compose_cmd: &crate::ComposeCommand,
    project: &ComposeProject,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let resolved = compose_cmd.config().await?;
    let service_info = crate::extract_service_build_info(&resolved, &project.primary_service)?;
    Ok(service_info.resolved_image_name(&project.project_name, &project.primary_service))
}
