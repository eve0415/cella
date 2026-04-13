//! Compose + features orchestration.
//!
//! Implements the combined-Dockerfile approach matching the devcontainer CLI:
//! the original service Dockerfile (or a synthetic one for image-only services)
//! is concatenated with feature installation layers, then the compose override
//! points `build.dockerfile` to this combined file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use cella_backend::{ContainerBackend, image_name_with_features};
use cella_compose::{
    ComposeCommand, ComposeProject, FEATURES_TARGET_STAGE, ServiceBuildInfo,
    extract_service_build_info,
};
use cella_features::ResolvedFeatures;

use crate::image::compute_features_digest;
use crate::progress::ProgressSender;

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
    /// The base image USER directive (for UID remap image layer).
    pub image_user: String,
    /// Raw `devcontainer.metadata` label from the base image, if available.
    pub base_image_metadata: Option<String>,
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
    progress: &ProgressSender,
) -> Result<Option<ComposeFeaturesBuild>, Box<dyn std::error::Error + Send + Sync>> {
    // 1. Check if features are present
    if !has_features(config) {
        return Ok(None);
    }

    info!("Compose + features detected, resolving combined Dockerfile");

    // 2. Resolve compose config via `docker compose config --format json`
    //    Use without_override because the override file hasn't been written yet.
    let compose_cmd = ComposeCommand::without_override(project);
    let resolved_compose = run_step_result(
        progress,
        "Resolving compose configuration...",
        compose_cmd.config(),
    )
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

            // Resolve the effective image user via parse → base-image inspect
            // → "root". Mirrors the devcontainer CLI's findUserStatement +
            // imageBuildInfoFromDockerfile fallback chain so the features
            // stage's final USER matches the Dockerfile's declared user.
            let (image_user, base_image_metadata) =
                resolve_build_image_user(client, &named_content, Some(&name), progress).await;

            (named_content, name, image_user, base_image_metadata)
        }
        ServiceBuildInfo::Image { image } => {
            // Pull image if needed for inspection
            if !client.image_exists(image).await? {
                run_step_result(
                    progress,
                    "Pulling compose service image...",
                    client.pull_image(image),
                )
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

    let resolved = run_step_result(progress, "Resolving devcontainer features...", async {
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
        &image_user,
    )?;

    // 7. Assemble build context overrides and return.
    Ok(Some(assemble_features_build(
        config,
        &service_info,
        project,
        combined_path,
        resolved,
        image_user,
        base_image_metadata,
    )?))
}

/// Resolve the effective USER and base-image metadata for a build-based
/// compose service.
///
/// Mirrors the devcontainer CLI's `internalGetImageBuildInfoFromDockerfile`:
/// always inspects the external base image (for `devcontainer.metadata`),
/// and picks the user as:
///
/// 1. Static parse of the Dockerfile target stage's last `USER` directive.
/// 2. The base image's `Config.User` from inspect.
/// 3. `"root"` fallback.
///
/// The base image's `devcontainer.metadata` label is returned regardless of
/// which source set `image_user`, so downstream `resolve_remote_user` can
/// apply the full spec precedence (config.remoteUser > config.containerUser >
/// image metadata remoteUser > image metadata containerUser > image USER).
async fn resolve_build_image_user(
    client: &dyn ContainerBackend,
    dockerfile_content: &str,
    target_stage: Option<&str>,
    progress: &ProgressSender,
) -> (String, Option<String>) {
    let parsed_user = cella_compose::find_user_statement(dockerfile_content, target_stage);

    let Some(base_image) = cella_compose::find_stage_base_image(dockerfile_content, target_stage)
    else {
        debug!("Could not resolve a base image to inspect for metadata");
        return (parsed_user.unwrap_or_else(|| "root".to_string()), None);
    };

    if matches!(client.image_exists(&base_image).await, Ok(false))
        && let Err(e) = run_step_result(
            progress,
            "Pulling base image for user inspection...",
            client.pull_image(&base_image),
        )
        .await
    {
        warn!("Failed to pull base image '{base_image}' for user inspection: {e}");
        return (parsed_user.unwrap_or_else(|| "root".to_string()), None);
    }

    match client.inspect_image_details(&base_image).await {
        Ok(details) => {
            let (user, metadata) = pick_image_user_and_metadata(
                parsed_user.as_deref(),
                Some(details.user.as_str()),
                details.metadata,
            );
            debug!(
                "Resolved image user: {user} (has metadata: {})",
                metadata.is_some()
            );
            (user, metadata)
        }
        Err(e) => {
            warn!("Failed to inspect base image '{base_image}': {e}");
            (parsed_user.unwrap_or_else(|| "root".to_string()), None)
        }
    }
}

/// Combine a Dockerfile-parsed USER with a base-image inspect result into the
/// final `(image_user, base_image_metadata)` tuple.
///
/// - `image_user` precedence: parser > inspect (non-empty) > `"root"`.
/// - `base_image_metadata` is returned as-is; the caller always wants it
///   when inspect succeeded, regardless of which source set `image_user`.
///
/// Extracted so it can be unit-tested without a `ContainerBackend` mock.
fn pick_image_user_and_metadata(
    parser_user: Option<&str>,
    inspect_user: Option<&str>,
    inspect_metadata: Option<String>,
) -> (String, Option<String>) {
    let inspect_nonempty = inspect_user.filter(|u| !u.is_empty());
    let user = parser_user
        .or(inspect_nonempty)
        .unwrap_or("root")
        .to_string();
    (user, inspect_metadata)
}

/// Generate the combined Dockerfile and write it to disk.
fn write_combined_dockerfile(
    project_name: &str,
    original_dockerfile: &str,
    features_dockerfile: &str,
    stage_name: &str,
    image_user: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let combined = cella_compose::generate_combined_dockerfile(
        original_dockerfile,
        features_dockerfile,
        stage_name,
        image_user,
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
    image_user: String,
    base_image_metadata: Option<String>,
) -> Result<ComposeFeaturesBuild, Box<dyn std::error::Error + Send + Sync>> {
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
            let features_digest = compute_features_digest(config);
            let img_name =
                image_name_with_features(&project.workspace_root, config_name, &features_digest);
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
        image_user,
        base_image_metadata,
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

#[cfg(test)]
mod tests {
    //! Regression tests for compose + features user resolution.
    //!
    //! Pure-function tests; the `resolve_build_image_user` paths that call
    //! `inspect_image_details` are covered by integration tests gated on the
    //! `integration-tests` feature of `cella-compose`.

    use cella_compose::{FEATURES_TARGET_STAGE, find_stage_base_image, find_user_statement};
    use cella_features::dockerfile::generate_dockerfile;

    /// End-to-end reproduction of the original bug: a Dockerfile ending with
    /// `USER node:node` plus features must produce a combined Dockerfile
    /// whose final `USER` instruction resolves to `node`, not `root`.
    #[test]
    fn combined_dockerfile_preserves_non_root_user_with_features() {
        // Representative user Dockerfile: ends with `USER node:node` and then
        // a few more non-USER instructions (like the reporter's case).
        let original = "\
FROM mcr.microsoft.com/devcontainers/typescript-node:4.0 AS base
RUN echo install-as-root
USER node:node
RUN echo install-as-node
";

        let resolved_user =
            find_user_statement(original, Some("base")).expect("USER node:node must be parseable");
        assert_eq!(resolved_user, "node");

        // Feature Dockerfile (no real features needed to exercise the
        // user-reset path — use a synthetic feature with an install script).
        let feature = cella_features::ResolvedFeature {
            id: "marker".to_string(),
            original_ref: "marker".to_string(),
            metadata: cella_features::FeatureMetadata::default(),
            user_options: std::collections::HashMap::new(),
            artifact_dir: std::path::PathBuf::from("/tmp/marker"),
            has_install_script: true,
        };
        let feature_dockerfile =
            generate_dockerfile("base", &resolved_user, "node", "node", &[feature], true);

        // Features target stage's final ARG must now be bare (no default),
        // so it inherits the global value.
        assert!(
            feature_dockerfile
                .contains("\nARG _DEV_CONTAINERS_IMAGE_USER\nUSER $_DEV_CONTAINERS_IMAGE_USER\n"),
            "features closing ARG must be bare to inherit from global: got\n{feature_dockerfile}"
        );
        assert!(
            !feature_dockerfile.contains("ARG _DEV_CONTAINERS_IMAGE_USER=root"),
            "features closing ARG must not shadow the global with =root"
        );

        let combined = cella_compose::generate_combined_dockerfile(
            original,
            &feature_dockerfile,
            "base",
            &resolved_user,
        );

        // Global ARG carries the resolved user.
        assert!(
            combined.contains("ARG _DEV_CONTAINERS_IMAGE_USER=node\n"),
            "global ARG must set _DEV_CONTAINERS_IMAGE_USER=node so the features stage inherits it"
        );

        // Global ARG precedes the first FROM (true global scope).
        let global_pos = combined
            .find("ARG _DEV_CONTAINERS_IMAGE_USER=node")
            .unwrap();
        let first_from_pos = combined.find("FROM ").unwrap();
        assert!(
            global_pos < first_from_pos,
            "global user ARG must appear before the first FROM"
        );

        // Features target stage is present and ends with the USER directive
        // that references the now-global-inherited ARG.
        assert!(combined.contains(&format!("AS {FEATURES_TARGET_STAGE}")));
        assert!(
            combined
                .trim_end()
                .ends_with("USER $_DEV_CONTAINERS_IMAGE_USER"),
            "combined Dockerfile must end on the inherited-USER line: got tail\n{}",
            combined
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// When the Dockerfile has no `USER` directive but the target stage's
    /// FROM references a resolvable external base image, `find_user_statement`
    /// returns None so the caller can fall back to image inspect.
    /// `find_stage_base_image` must return that external image reference.
    #[test]
    fn fallback_path_exposes_external_base_image() {
        let dockerfile = "\
FROM mcr.microsoft.com/devcontainers/base:ubuntu AS base
RUN echo nothing-user-related
";
        assert_eq!(find_user_statement(dockerfile, Some("base")), None);
        assert_eq!(
            find_stage_base_image(dockerfile, Some("base")),
            Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string())
        );
    }

    /// When the target stage has no `USER` and its FROM is a variable
    /// reference we cannot resolve statically, the helpers hand back None
    /// for both directions so the caller ends up using the `"root"` fallback.
    #[test]
    fn fallback_chain_terminates_on_variable_from() {
        let dockerfile = "\
ARG BASE_IMAGE=ubuntu:22.04
FROM $BASE_IMAGE AS base
RUN echo hi
";
        assert_eq!(find_user_statement(dockerfile, Some("base")), None);
        assert_eq!(find_stage_base_image(dockerfile, Some("base")), None);
    }

    /// Target stage with `USER numeric:gid` should be parsed and stripped
    /// down to just the user portion — this is what the UID remap `sed`
    /// pattern expects.
    #[test]
    fn numeric_user_group_is_stripped() {
        let dockerfile = "FROM alpine:3.18 AS base\nUSER 1000:1000\n";
        assert_eq!(
            find_user_statement(dockerfile, Some("base")),
            Some("1000".to_string())
        );
    }

    // ---------------------------------------------------------------
    // pick_image_user_and_metadata
    //
    // Regression coverage for the Codex stop-time review: the parser-hit
    // path must NOT drop the base image's devcontainer.metadata label. The
    // helper always returns the inspect metadata when inspect succeeded,
    // regardless of which source set image_user.
    // ---------------------------------------------------------------

    use super::pick_image_user_and_metadata;

    #[test]
    fn picker_parser_wins_and_metadata_preserved() {
        // Parser found USER node — inspect user is vscode. Parser wins for
        // image_user, BUT the metadata from inspect must still be returned
        // so resolve_remote_user can apply the full spec precedence.
        let (user, metadata) = pick_image_user_and_metadata(
            Some("node"),
            Some("vscode"),
            Some("{\"remoteUser\":\"vscode\"}".to_string()),
        );
        assert_eq!(user, "node");
        assert_eq!(
            metadata.as_deref(),
            Some("{\"remoteUser\":\"vscode\"}"),
            "metadata must survive the parser-hit path"
        );
    }

    #[test]
    fn picker_falls_back_to_inspect_user_when_parser_empty() {
        let (user, metadata) =
            pick_image_user_and_metadata(None, Some("vscode"), Some("{\"k\":1}".to_string()));
        assert_eq!(user, "vscode");
        assert_eq!(metadata.as_deref(), Some("{\"k\":1}"));
    }

    #[test]
    fn picker_falls_back_to_root_when_both_empty() {
        let (user, metadata) = pick_image_user_and_metadata(None, Some(""), None);
        assert_eq!(user, "root");
        assert!(metadata.is_none());
    }

    #[test]
    fn picker_ignores_empty_inspect_user() {
        // Empty Config.User (most Docker images) must not short-circuit to
        // "" — we need to fall back to the final `"root"` default.
        let (user, _) = pick_image_user_and_metadata(None, Some(""), None);
        assert_eq!(user, "root");
    }

    #[test]
    fn picker_returns_metadata_even_with_root_user() {
        // Base image USER=root but has metadata (e.g. mcr devcontainers with
        // spec-compliant metadata label). Metadata must flow through.
        let (user, metadata) = pick_image_user_and_metadata(
            None,
            Some("root"),
            Some("{\"containerUser\":\"node\"}".to_string()),
        );
        assert_eq!(user, "root");
        assert_eq!(metadata.as_deref(), Some("{\"containerUser\":\"node\"}"));
    }
}
