//! Compose + features orchestration.
//!
//! Implements the combined-Dockerfile approach matching the devcontainer CLI:
//! the original service Dockerfile (or a synthetic one for image-only services)
//! is concatenated with feature installation layers, then the compose override
//! points `build.dockerfile` to this combined file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::{debug, info};

use cella_backend::ContainerBackend;
use cella_compose::{
    ComposeCommand, ComposeProject, FEATURES_TARGET_STAGE, ServiceBuildInfo,
    extract_service_build_info,
};
use cella_features::ResolvedFeatures;

use crate::progress::Progress;

/// Result of resolving features for a compose service.
pub struct ComposeFeaturesBuild {
    /// Path to the combined Dockerfile (original + feature layers).
    pub combined_dockerfile: PathBuf,
    /// Build target stage name (`dev_containers_target_stage`).
    pub build_target: String,
    /// Build context override (empty dir for image-only, `None` for build-based).
    pub build_context: Option<PathBuf>,
    /// Named build contexts for Docker `BuildKit` `additional_contexts`.
    pub additional_contexts: BTreeMap<String, PathBuf>,
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
    client: &dyn ContainerBackend,
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
    let backend_platform = ContainerBackend::detect_platform(client)
        .await
        .map_err(|e| format!("platform detection failed: {e}"))?;
    let platform =
        cella_features::oci::detect_platform(&backend_platform.os, &backend_platform.arch);
    let cache = cella_features::FeatureCache::new();

    let resolved = progress
        .run_step_result("Resolving devcontainer features...", async {
            cella_features::resolve_features(
                config,
                config_path,
                &platform,
                &cache,
                &cella_features::BaseImageContext {
                    base_image: &stage_name,
                    image_user: &image_user,
                    metadata: base_image_metadata.as_deref(),
                },
                true, // compose builds use named content source via additional_contexts
            )
            .await
            .map_err(|e| format!("feature resolution failed: {e}"))
        })
        .await?;

    // 6. Generate combined Dockerfile and write to disk.
    let combined_path = write_combined_dockerfile(
        &project.project_name,
        &original_dockerfile,
        &resolved.dockerfile,
        &stage_name,
    )?;

    // 7. Assemble build context overrides and return.
    Ok(Some(assemble_features_build(
        config,
        &service_info,
        project,
        combined_path,
        resolved,
    )?))
}

/// Generate the combined Dockerfile and write it to disk.
fn write_combined_dockerfile(
    project_name: &str,
    original_dockerfile: &str,
    features_dockerfile: &str,
    stage_name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let combined = cella_compose::generate_combined_dockerfile(
        original_dockerfile,
        features_dockerfile,
        stage_name,
    );
    let combined_path = compose_dockerfile_path(project_name);
    if let Some(parent) = combined_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&combined_path, &combined)?;
    info!(
        "Combined Dockerfile written to: {}",
        combined_path.display()
    );
    Ok(combined_path)
}

/// Assemble the `ComposeFeaturesBuild` with build context overrides.
fn assemble_features_build(
    config: &serde_json::Value,
    service_info: &ServiceBuildInfo,
    project: &ComposeProject,
    combined_dockerfile: PathBuf,
    resolved: ResolvedFeatures,
) -> Result<ComposeFeaturesBuild, Box<dyn std::error::Error>> {
    let mut additional_contexts = BTreeMap::new();
    additional_contexts.insert(
        cella_features::FEATURE_CONTENT_SOURCE.to_string(),
        resolved.build_context.clone(),
    );

    let (build_context, image_name_override) = match service_info {
        ServiceBuildInfo::Image { .. } => {
            let empty_context = compose_empty_context_path(&project.project_name);
            std::fs::create_dir_all(&empty_context)?;

            let config_name = config.get("name").and_then(|v| v.as_str());
            let features_digest = super::image::compute_features_digest(config);
            let img_name = cella_docker::image_name_with_features(
                &project.workspace_root,
                config_name,
                &features_digest,
            );
            (Some(empty_context), Some(img_name))
        }
        ServiceBuildInfo::Build { .. } => (None, None),
    };

    Ok(ComposeFeaturesBuild {
        combined_dockerfile,
        build_target: FEATURES_TARGET_STAGE.to_string(),
        build_context,
        additional_contexts,
        resolved_features: resolved,
        image_name_override,
    })
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

/// Compute the path for the empty build context directory (image-only services).
fn compose_empty_context_path(project_name: &str) -> PathBuf {
    cella_data_dir()
        .join("compose")
        .join(project_name)
        .join("empty-context")
}

/// Get the cella data directory (`~/.cella/`).
fn cella_data_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join(".cella")
}
