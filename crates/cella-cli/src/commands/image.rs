use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::info;

use cella_docker::{BuildOptions, DockerClient, image_name, image_name_with_features};
use cella_features::ResolvedFeatures;

/// Compute a SHA-256 digest of the features config for image tagging.
pub fn compute_features_digest(config: &serde_json::Value) -> String {
    let features = config.get("features").unwrap_or(&serde_json::Value::Null);
    let canonical = serde_json::to_string(features).unwrap_or_default();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Build the features layer image on top of a base image.
pub async fn build_features_layer(
    client: &DockerClient,
    config: &serde_json::Value,
    workspace_root: &Path,
    config_name: Option<&str>,
    resolved: &ResolvedFeatures,
    base_image: &str,
    no_cache: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let features_digest = compute_features_digest(config);
    let features_image = image_name_with_features(workspace_root, config_name, &features_digest);

    let mut options = vec![];
    if no_cache {
        options.push("--no-cache".to_string());
    }

    let mut args = HashMap::new();
    args.insert(
        "_DEV_CONTAINERS_BASE_IMAGE".to_string(),
        base_image.to_string(),
    );

    let build_opts = BuildOptions {
        image_name: features_image.clone(),
        context_path: resolved.build_context.clone(),
        dockerfile: "Dockerfile.features".to_string(),
        args,
        target: None,
        cache_from: vec![],
        options,
    };

    info!(
        "Building features layer image (context: {})",
        resolved.build_context.display()
    );
    client.build_image(&build_opts).await?;
    Ok(features_image)
}

/// Ensure the dev container image exists (pull or build), including features layer.
///
/// When `no_cache` is true, `--no-cache` and `--pull` are passed to the base
/// image build (but only `--no-cache` for the features layer, since its FROM
/// image is local-only) and image-based configs force re-pull.
#[allow(clippy::too_many_lines)]
pub async fn ensure_image(
    client: &DockerClient,
    config: &serde_json::Value,
    workspace_root: &Path,
    config_name: Option<&str>,
    config_path: &Path,
    no_cache: bool,
) -> Result<(String, Option<ResolvedFeatures>), Box<dyn std::error::Error>> {
    let has_features = config
        .get("features")
        .and_then(|v| v.as_object())
        .is_some_and(|obj| !obj.is_empty());

    // Determine base image tag
    let base_image_tag = if let Some(image) = config.get("image").and_then(|v| v.as_str()) {
        // Pull base image if needed (force re-pull when no_cache)
        if no_cache || !client.image_exists(image).await? {
            client.pull_image(image).await?;
        }
        image.to_string()
    } else if let Some(build) = config.get("build").and_then(|v| v.as_object()) {
        // Build user Dockerfile
        let img_name = image_name(workspace_root, config_name);
        let build_opts = parse_build_options(build, &img_name, workspace_root, no_cache);
        client.build_image(&build_opts).await?;
        img_name
    } else {
        return Err("devcontainer.json must specify either 'image' or 'build'".into());
    };

    // If no features, return the base image directly
    if !has_features {
        return Ok((base_image_tag, None));
    }

    // Resolve features
    info!("Resolving devcontainer features...");
    let platform = cella_features::oci::detect_platform(client.inner())
        .await
        .map_err(|e| format!("platform detection failed: {e}"))?;
    let cache = cella_features::FeatureCache::new();
    let image_user = client.inspect_image_user(&base_image_tag).await?;

    let resolved = cella_features::resolve_features(
        config,
        config_path,
        &platform,
        &cache,
        &base_image_tag,
        &image_user,
    )
    .await
    .map_err(|e| format!("feature resolution failed: {e}"))?;

    // Build the features layer image
    let features_image = build_features_layer(
        client,
        config,
        workspace_root,
        config_name,
        &resolved,
        &base_image_tag,
        no_cache,
    )
    .await?;

    Ok((features_image, Some(resolved)))
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
