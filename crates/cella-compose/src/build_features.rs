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
    /// Image labels (`key=value`) from `cella build --label`, applied as the
    /// primary service's `build.labels` so they bake into the built image
    /// (mirrors the single-container `docker build --label`). Empty = no labels.
    /// An image-only service (no build, no features) has nothing to label and is
    /// rejected up front. Pre-validated `key=value` strings (non-empty key).
    pub labels: Vec<String>,
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
            // `--label` image labels join the combined-Dockerfile build, so the
            // final image carries them (parity with single-container --label).
            build_labels: cfg.labels.clone(),
            extra_volumes: Vec::new(),
            // GPU reservation is emitted only in the final override.
            request_gpu: false,
            // `cella build` only builds the image; runtime security props apply at `up`.
            security: cella_config::config_map::MergedSecurityConfig::default(),
            // This override persists and is reused at `up`, where the container
            // runs and needs the agent volume — keep the runtime sections.
            build_only: false,
        };
        let yaml = crate::override_file::generate_override_yaml(&override_config);
        crate::override_file::write_override_file(&project.override_file, &yaml)?;
    }

    let had_features = features_build.is_some();
    let image_name = if let Some(fb) = features_build {
        // Features path: the override (with any `--label` build.labels) is written
        // above, so build with it and use its image-name override when present, or
        // resolve the default `{project}-{service}` via the same override command.
        let compose_cmd = crate::ComposeCommand::new(&project)
            .with_docker_binaries(cfg.docker_path.clone(), cfg.docker_compose_path.clone());
        run_compose_build(&compose_cmd).await?;
        match fb.image_name_override {
            Some(image) => image,
            None => resolve_primary_service_image(&compose_cmd, &project).await?,
        }
    } else {
        // No-features path. With no `--label`, build and resolve both run
        // `without_override` (no override file exists) — unchanged. With
        // `--label`, a build-based service gets a labels-only override for the
        // build only, while the image name is still resolved off `without_override`
        // data so the #192 invariant holds (the labels override never feeds
        // `resolve_primary_service_image`/`docker compose config`).
        build_no_features(&project, cfg).await?
    };

    Ok(ComposeBuildResult {
        image_name,
        had_features,
    })
}

/// Run `docker compose build` for the primary service, mapping failures to a
/// uniform error.
async fn run_compose_build(
    compose_cmd: &crate::ComposeCommand,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    compose_cmd
        .build(None, false)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("docker compose build failed: {e}").into()
        })
}

/// Build the no-features compose path and resolve the primary service's image.
///
/// Two shapes, gated on `--label`:
/// - **No labels:** build and resolve both run on `without_override` (the
///   override file is never written) — byte-for-byte the pre-`--label` behavior.
/// - **Labels:** the service must build (sub-case 2). A bare `image:` service has
///   no build to attach labels to (sub-case 3) → a clear error. For a build-based
///   service, a labels-only override (only `build.labels`; dockerfile/context are
///   inherited from the base compose via `-f` merge) is written and used for the
///   BUILD only. The image NAME is resolved from the `ServiceBuildInfo` already
///   fetched off `without_override`, so the labels override never reaches
///   `docker compose config` — preserving the #192 `resolve_primary_service_image`
///   invariant.
async fn build_no_features(
    project: &ComposeProject,
    cfg: &ComposeBuildConfig<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let plain_cmd = crate::ComposeCommand::without_override(project)
        .with_docker_binaries(cfg.docker_path.clone(), cfg.docker_compose_path.clone());

    if cfg.labels.is_empty() {
        run_compose_build(&plain_cmd).await?;
        return resolve_primary_service_image(&plain_cmd, project).await;
    }

    // `--label` set: resolve the service shape up front (off `without_override`,
    // never the labels override) to both gate image-only services out and reuse
    // the result for the final image name.
    let resolved = plain_cmd.config().await?;
    let service_info = crate::extract_service_build_info(&resolved, &project.primary_service)?;
    if !service_build_can_be_labeled(&service_info) {
        return Err(image_only_label_error().into());
    }

    write_labels_only_override(project, &cfg.labels)?;
    let labeled_build_cmd = crate::ComposeCommand::new(project)
        .with_docker_binaries(cfg.docker_path.clone(), cfg.docker_compose_path.clone());
    run_compose_build(&labeled_build_cmd).await?;

    Ok(service_info.resolved_image_name(&project.project_name, &project.primary_service))
}

/// Whether a no-features service can carry `--label` image labels.
///
/// `--label` lands on a built image via `build.labels`, so only a service that
/// actually builds can be labeled. A bare `image:` service is used as-is (Compose
/// neither builds nor re-tags it), so it has no build to attach labels to.
const fn service_build_can_be_labeled(info: &crate::ServiceBuildInfo) -> bool {
    matches!(info, crate::ServiceBuildInfo::Build { .. })
}

/// Error message for `--label` on a no-features, image-only compose service.
///
/// Mirrors the single-container bare-`image:` boundary (both error rather than
/// silently dropping the flag).
fn image_only_label_error() -> String {
    "--label requires a built service; an image-only compose service is used as-is \
     and cannot be labeled."
        .to_string()
}

/// Write a minimal override carrying only the primary service's `build.labels`.
///
/// Minimal in the build sense: no dockerfile/context/target/secrets — those are
/// inherited from the base compose via the `-f` merge; this override exists solely
/// to attach image labels to the build. It is marked `build_only`, so it omits the
/// runtime sections (the agent volume mount and the top-level `volumes:` block):
/// `cella build --label` only builds the image and never provisions the agent
/// volume, so an override that declared it as an `external` volume could trip
/// compose's external-volume validation on a fresh machine. Omitting it entirely
/// keeps the override valid regardless. Used for the no-features, build-based
/// `--label` path.
fn write_labels_only_override(
    project: &ComposeProject,
    labels: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let override_config = crate::OverrideConfig {
        primary_service: project.primary_service.clone(),
        image_override: None,
        override_command: false,
        // Omitted from output (build_only); the build never mounts the volume.
        agent_volume_name: String::new(),
        agent_volume_target: String::new(),
        extra_env: Vec::new(),
        extra_labels: std::collections::BTreeMap::new(),
        build_dockerfile: None,
        build_target: None,
        build_context: None,
        additional_contexts: std::collections::BTreeMap::new(),
        build_secrets: Vec::new(),
        build_labels: labels.to_vec(),
        extra_volumes: Vec::new(),
        request_gpu: false,
        security: cella_config::config_map::MergedSecurityConfig::default(),
        build_only: true,
    };
    let yaml = crate::override_file::generate_override_yaml(&override_config);
    crate::override_file::write_override_file(&project.override_file, &yaml)?;
    Ok(())
}

/// Resolve the primary service's image name from the resolved compose config.
///
/// Runs `docker compose config` through `compose_cmd` and maps the primary
/// service's build/image info to the name it resolves to after a build (the
/// service's `image:` reference, or `{project}-{service}` for a build-based
/// service without one). Pass the same `compose_cmd` used for the preceding
/// build: it must not reference a non-existent override file (the no-features
/// path uses `without_override`; on the features path the override exists by the
/// time this runs).
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::{image_only_label_error, service_build_can_be_labeled};
    use crate::ServiceBuildInfo;

    #[test]
    fn build_service_can_be_labeled() {
        // A build-based service has a build to attach `--label` image labels to.
        let info = ServiceBuildInfo::Build {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".to_string(),
            target: None,
            args: HashMap::new(),
            image: None,
        };
        assert!(service_build_can_be_labeled(&info));
    }

    #[test]
    fn build_service_with_image_tag_can_be_labeled() {
        // A `build:` + `image:` service still builds (Compose tags the output),
        // so `--label` applies to its build.
        let info = ServiceBuildInfo::Build {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".to_string(),
            target: None,
            args: HashMap::new(),
            image: Some("myapp:latest".to_string()),
        };
        assert!(service_build_can_be_labeled(&info));
    }

    #[test]
    fn image_only_service_cannot_be_labeled() {
        // A bare `image:` service is used as-is — no build, nothing to label.
        let info = ServiceBuildInfo::Image {
            image: "alpine:3.21".to_string(),
        };
        assert!(!service_build_can_be_labeled(&info));
    }

    #[test]
    fn image_only_label_error_message_matches_boundary() {
        // Stable, user-facing message mirroring the single-container bare-`image:`
        // boundary. Pinned so the wording can't silently drift.
        assert_eq!(
            image_only_label_error(),
            "--label requires a built service; an image-only compose service is used as-is \
             and cannot be labeled."
        );
    }
}
