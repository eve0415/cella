//! Compose + features orchestration.
//!
//! Implements the combined-Dockerfile approach matching the devcontainer CLI:
//! the original service Dockerfile (or a synthetic one for image-only services)
//! is concatenated with feature installation layers, then the compose override
//! points `build.dockerfile` to this combined file.

use std::path::{Path, PathBuf};

use tracing::{debug, info};

use cella_compose::{
    ComposeCommand, ComposeProject, FEATURES_TARGET_STAGE, ServiceBuildInfo,
    extract_service_build_info,
};
use cella_docker::DockerClient;
use cella_features::ResolvedFeatures;

use crate::progress::Progress;

/// Result of resolving features for a compose service.
pub struct ComposeFeaturesBuild {
    /// Path to the combined Dockerfile (original + feature layers).
    pub combined_dockerfile: PathBuf,
    /// Build target stage name (`dev_containers_target_stage`).
    pub build_target: String,
    /// Build context override (only for image-only services).
    pub build_context: Option<PathBuf>,
    /// Resolved features (for lifecycle metadata, labels, etc.).
    pub resolved_features: ResolvedFeatures,
    /// Image name override (for image-only services, to avoid retagging).
    pub image_name_override: Option<String>,
}

/// Resolve features for a compose service and generate the combined Dockerfile.
///
/// Returns `None` if no features are configured.
///
/// # Flow
///
/// 1. Check if features are present; return `None` if not
/// 2. Run `docker compose config --format json` to resolve compose config
/// 3. Extract primary service build/image info
/// 4. Read or synthesize the original Dockerfile, ensure named stage
/// 5. Resolve features using the stage name as the base image
/// 6. Generate combined Dockerfile and write to disk
///
/// # Errors
///
/// Returns an error if compose config resolution fails, the Dockerfile
/// cannot be read, or feature resolution fails.
pub async fn resolve_compose_features(
    client: &DockerClient,
    config: &serde_json::Value,
    config_path: &Path,
    project: &ComposeProject,
    _no_cache: bool,
    progress: &Progress,
) -> Result<Option<ComposeFeaturesBuild>, Box<dyn std::error::Error>> {
    // 1. Check if features are present
    if !has_features(config) {
        return Ok(None);
    }

    info!("Compose + features detected, resolving combined Dockerfile");

    // 2. Resolve compose config via `docker compose config --format json`
    //    Use without_override because the override file hasn't been written yet.
    let compose_cmd = ComposeCommand::without_override(project);
    let resolved_compose = progress
        .run_step_result("Resolving compose configuration...", compose_cmd.config())
        .await?;

    // 3. Extract primary service build info
    let service_info = extract_service_build_info(&resolved_compose, &project.primary_service)?;

    // 4. Get original Dockerfile content and stage name + image metadata
    let (original_dockerfile, stage_name, image_user, base_image_metadata) = match &service_info {
        ServiceBuildInfo::Build {
            context,
            dockerfile,
            target,
            ..
        } => {
            let dockerfile_path = context.join(dockerfile);
            let content = std::fs::read_to_string(&dockerfile_path).map_err(|e| {
                format!(
                    "failed to read Dockerfile at {}: {e}",
                    dockerfile_path.display()
                )
            })?;

            let (named_content, name) =
                cella_compose::ensure_stage_named(&content, target.as_deref())?;

            debug!(
                "Original Dockerfile from compose service (stage: {name}): {}",
                dockerfile_path.display()
            );

            // For build-based services, we can't easily inspect the image user
            // before building. Default to "root" — features resolve the correct
            // user from the image metadata after build, and the devcontainer CLI
            // also defaults to "root" for Dockerfile-based services.
            (named_content, name, "root".to_string(), None)
        }
        ServiceBuildInfo::Image { image } => {
            // Pull image if needed for inspection
            if !client.image_exists(image).await? {
                progress
                    .run_step_result("Pulling compose service image...", client.pull_image(image))
                    .await?;
            }

            let details = client.inspect_image_details(image).await?;
            let (content, name) = cella_compose::synthetic_dockerfile(image);

            debug!("Synthetic Dockerfile for image-only compose service: {image}");

            (content, name, details.user.clone(), details.metadata)
        }
    };

    // 5. Resolve features using stage name as the base image reference
    let platform = cella_features::oci::detect_platform(client.inner())
        .await
        .map_err(|e| format!("platform detection failed: {e}"))?;
    let cache = cella_features::FeatureCache::new();

    let resolved = progress
        .run_step_result("Resolving devcontainer features...", async {
            cella_features::resolve_features(
                config,
                config_path,
                &platform,
                &cache,
                &stage_name,
                &image_user,
                base_image_metadata.as_deref(),
            )
            .await
            .map_err(|e| format!("feature resolution failed: {e}"))
        })
        .await?;

    // 6. Generate combined Dockerfile
    let combined = cella_compose::generate_combined_dockerfile(
        &original_dockerfile,
        &resolved.dockerfile,
        &stage_name,
    );

    // Write to project-specific directory
    let combined_path = compose_dockerfile_path(&project.project_name);
    if let Some(parent) = combined_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&combined_path, &combined)?;
    info!(
        "Combined Dockerfile written to: {}",
        combined_path.display()
    );

    // 7. Determine build context and image name overrides
    let (build_context, image_name_override) = match &service_info {
        ServiceBuildInfo::Image { .. } => {
            // For image-only services, we need to provide the features build
            // context (which contains the feature install scripts) and override
            // the image name to avoid retagging the original.
            let config_name = config.get("name").and_then(|v| v.as_str());
            let features_digest = super::image::compute_features_digest(config);
            let img_name = cella_docker::image_name_with_features(
                &project.workspace_root,
                config_name,
                &features_digest,
            );
            (Some(resolved.build_context.clone()), Some(img_name))
        }
        ServiceBuildInfo::Build { .. } => {
            // For build-based services, the original context/args are inherited
            // from the compose file. No context or image override needed.
            (None, None)
        }
    };

    Ok(Some(ComposeFeaturesBuild {
        combined_dockerfile: combined_path,
        build_target: FEATURES_TARGET_STAGE.to_string(),
        build_context,
        resolved_features: resolved,
        image_name_override,
    }))
}

/// Check if the config has non-empty features.
fn has_features(config: &serde_json::Value) -> bool {
    config
        .get("features")
        .is_some_and(|v| v.is_object() && !v.as_object().unwrap().is_empty())
}

/// Compute the path for the combined Dockerfile.
fn compose_dockerfile_path(project_name: &str) -> PathBuf {
    cella_data_dir()
        .join("compose")
        .join(project_name)
        .join("Dockerfile.combined")
}

/// Get the cella data directory (`~/.cella/`).
fn cella_data_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join(".cella")
}
