use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::info;

use cella_docker::{
    BuildOptions, DockerClient, ImageDetails, image_name, image_name_with_features,
};
use cella_features::ResolvedFeatures;

use crate::progress::Progress;

/// Compute a SHA-256 digest of the features config for image tagging.
pub fn compute_features_digest(config: &serde_json::Value) -> String {
    let features = config.get("features").unwrap_or(&serde_json::Value::Null);
    let canonical = serde_json::to_string(features).unwrap_or_default();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Context for building a features layer image.
pub struct FeaturesLayerContext<'a> {
    pub client: &'a DockerClient,
    pub config: &'a serde_json::Value,
    pub workspace_root: &'a Path,
    pub config_name: Option<&'a str>,
    pub resolved: &'a ResolvedFeatures,
    pub base_image: &'a str,
    pub image_user: &'a str,
    pub no_cache: bool,
    pub progress: &'a Progress,
}

/// Build the features layer image on top of a base image.
pub async fn build_features_layer(
    ctx: &FeaturesLayerContext<'_>,
) -> Result<String, Box<dyn std::error::Error>> {
    let features_digest = compute_features_digest(ctx.config);
    let features_image =
        image_name_with_features(ctx.workspace_root, ctx.config_name, &features_digest);

    let mut options = vec![];
    if ctx.no_cache {
        options.push("--no-cache".to_string());
    }

    let mut args = HashMap::new();
    args.insert(
        "_DEV_CONTAINERS_BASE_IMAGE".to_string(),
        ctx.base_image.to_string(),
    );
    args.insert(
        "_DEV_CONTAINERS_IMAGE_USER".to_string(),
        ctx.image_user.to_string(),
    );

    let build_opts = BuildOptions {
        image_name: features_image.clone(),
        context_path: ctx.resolved.build_context.clone(),
        dockerfile: "Dockerfile.features".to_string(),
        args,
        target: None,
        cache_from: vec![],
        options,
    };

    info!(
        "Building features layer image (context: {})",
        ctx.resolved.build_context.display()
    );
    let start = std::time::Instant::now();
    let progress_ref = ctx.progress.clone();
    ctx.progress
        .println("  \x1b[36m▸\x1b[0m Building features layer...");
    let result = ctx
        .client
        .build_image(&build_opts, |line| {
            if !line.trim().is_empty() {
                progress_ref.println(&format!("      {line}"));
            }
        })
        .await;
    let elapsed = crate::progress::format_elapsed_pub(start.elapsed());
    match &result {
        Ok(_) => ctx
            .progress
            .println(&format!("  \x1b[32m✓\x1b[0m Built features layer{elapsed}")),
        Err(e) => ctx
            .progress
            .println(&format!("  \x1b[31m✗\x1b[0m Building features layer: {e}")),
    }
    result?;
    Ok(features_image)
}

/// Ensure the dev container image exists (pull or build), including features layer.
///
/// When `no_cache` is true, `--no-cache` and `--pull` are passed to the base
/// image build (but only `--no-cache` for the features layer, since its FROM
/// image is local-only) and image-based configs force re-pull.
pub async fn ensure_image(
    client: &DockerClient,
    config: &serde_json::Value,
    workspace_root: &Path,
    config_name: Option<&str>,
    config_path: &Path,
    no_cache: bool,
    progress: &Progress,
) -> Result<(String, Option<ResolvedFeatures>, ImageDetails), Box<dyn std::error::Error>> {
    let has_features = config
        .get("features")
        .and_then(|v| v.as_object())
        .is_some_and(|obj| !obj.is_empty());

    // Determine base image tag
    let base_image_tag = resolve_base_image(
        client,
        config,
        workspace_root,
        config_name,
        no_cache,
        progress,
    )
    .await?;

    // Inspect base image details (user, env, metadata) in a single API call
    let base_image_details = client.inspect_image_details(&base_image_tag).await?;

    // If no features, return the base image directly
    if !has_features {
        return Ok((base_image_tag, None, base_image_details));
    }

    // Resolve and build features layer
    let input = FeaturesBuildInput {
        client,
        config,
        config_path,
        workspace_root,
        config_name,
        base_image_tag: &base_image_tag,
        base_image_details: &base_image_details,
        no_cache,
        progress,
    };
    let (features_image, resolved) = resolve_and_build_features(&input).await?;

    Ok((features_image, Some(resolved), base_image_details))
}

/// Pull or build the base image and return its tag.
async fn resolve_base_image(
    client: &DockerClient,
    config: &serde_json::Value,
    workspace_root: &Path,
    config_name: Option<&str>,
    no_cache: bool,
    progress: &Progress,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(image) = config.get("image").and_then(|v| v.as_str()) {
        // Pull base image if needed (force re-pull when no_cache)
        if no_cache || !client.image_exists(image).await? {
            progress
                .run_step("Pulling base image...", client.pull_image(image))
                .await?;
        }
        Ok(image.to_string())
    } else if let Some(build) = config.get("build").and_then(|v| v.as_object()) {
        let img_name = image_name(workspace_root, config_name);
        let build_opts = parse_build_options(build, &img_name, workspace_root, no_cache);
        let start = std::time::Instant::now();
        let progress_ref = progress.clone();
        progress.println("  \x1b[36m▸\x1b[0m Building Dockerfile...");
        let result = client
            .build_image(&build_opts, |line| {
                if !line.trim().is_empty() {
                    progress_ref.println(&format!("      {line}"));
                }
            })
            .await;
        let elapsed = crate::progress::format_elapsed_pub(start.elapsed());
        match &result {
            Ok(_) => progress.println(&format!("  \x1b[32m✓\x1b[0m Built Dockerfile{elapsed}")),
            Err(e) => progress.println(&format!("  \x1b[31m✗\x1b[0m Building Dockerfile: {e}")),
        }
        result?;
        Ok(img_name)
    } else {
        Err("devcontainer.json must specify either 'image' or 'build'".into())
    }
}

/// Inputs for resolving and building the features layer image.
struct FeaturesBuildInput<'a> {
    client: &'a DockerClient,
    config: &'a serde_json::Value,
    config_path: &'a Path,
    workspace_root: &'a Path,
    config_name: Option<&'a str>,
    base_image_tag: &'a str,
    base_image_details: &'a ImageDetails,
    no_cache: bool,
    progress: &'a Progress,
}

/// Resolve features and build the features layer image.
async fn resolve_and_build_features(
    input: &FeaturesBuildInput<'_>,
) -> Result<(String, ResolvedFeatures), Box<dyn std::error::Error>> {
    info!("Resolving devcontainer features...");
    let platform = cella_features::oci::detect_platform(input.client.inner())
        .await
        .map_err(|e| format!("platform detection failed: {e}"))?;
    let cache = cella_features::FeatureCache::new();

    let resolved = cella_features::resolve_features(
        input.config,
        input.config_path,
        &platform,
        &cache,
        input.base_image_tag,
        &input.base_image_details.user,
        input.base_image_details.metadata.as_deref(),
    )
    .await
    .map_err(|e| format!("feature resolution failed: {e}"))?;

    let ctx = FeaturesLayerContext {
        client: input.client,
        config: input.config,
        workspace_root: input.workspace_root,
        config_name: input.config_name,
        resolved: &resolved,
        base_image: input.base_image_tag,
        image_user: &input.base_image_details.user,
        no_cache: input.no_cache,
        progress: input.progress,
    };
    let features_image = build_features_layer(&ctx).await?;

    Ok((features_image, resolved))
}

/// Parse build configuration from the `build` object in devcontainer.json.
pub fn parse_build_options(
    build: &serde_json::Map<String, serde_json::Value>,
    img_name: &str,
    workspace_root: &Path,
    no_cache: bool,
) -> BuildOptions {
    let dockerfile = build
        .get("dockerfile")
        .and_then(|v| v.as_str())
        .unwrap_or("Dockerfile")
        .to_string();

    let context = build.get("context").and_then(|v| v.as_str()).unwrap_or(".");

    let context_path = if Path::new(context).is_absolute() {
        PathBuf::from(context)
    } else {
        workspace_root.join(".devcontainer").join(context)
    };

    let args: HashMap<String, String> = build
        .get("args")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let target = build
        .get("target")
        .and_then(|v| v.as_str())
        .map(String::from);

    let cache_from: Vec<String> = build
        .get("cacheFrom")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut options: Vec<String> = build
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if no_cache {
        options.extend(["--no-cache".to_string(), "--pull".to_string()]);
    }

    BuildOptions {
        image_name: img_name.to_string(),
        context_path,
        dockerfile,
        args,
        target,
        cache_from,
        options,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_build_options_no_cache_adds_flags() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "test:latest", Path::new("/ws"), true);
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_without_no_cache() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "test:latest", Path::new("/ws"), false);
        assert!(!opts.options.contains(&"--no-cache".to_string()));
        assert!(!opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_preserves_existing_options() {
        let build: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{"dockerfile": "Dockerfile", "context": ".", "options": ["--squash"]}"#,
        )
        .unwrap();
        let opts = parse_build_options(&build, "test:latest", Path::new("/ws"), true);
        assert!(opts.options.contains(&"--squash".to_string()));
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
    }
}
