//! Container image resolution: pull, build, and features layer.
//!
//! Moved from `cella-cli/src/commands/image.rs`. Uses [`ProgressSender`]
//! instead of the indicatif-coupled `Progress` type.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::info;

use cella_backend::{
    BuildOptions, BuildSecret, ContainerBackend, ImageDetails, compute_features_digest, image_name,
    image_name_with_features,
};
use cella_features::ResolvedFeatures;

use crate::progress::{ProgressSender, format_elapsed};

/// Context for building a features layer image.
pub struct FeaturesLayerContext<'a> {
    pub client: &'a dyn ContainerBackend,
    pub config: &'a serde_json::Value,
    pub workspace_root: &'a Path,
    pub config_name: Option<&'a str>,
    pub resolved: &'a ResolvedFeatures,
    pub base_image: &'a str,
    pub image_user: &'a str,
    pub no_cache: bool,
    pub progress: &'a ProgressSender,
}

/// Build the features layer image on top of a base image.
///
/// # Errors
///
/// Returns an error if the Docker build fails or the build context is invalid.
pub async fn build_features_layer(
    ctx: &FeaturesLayerContext<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
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
        secrets: vec![],
    };

    info!(
        "Building features layer image (context: {})",
        ctx.resolved.build_context.display()
    );
    let start = std::time::Instant::now();
    ctx.progress
        .println("  \x1b[36m▸\x1b[0m Building features layer...");
    let result = ctx.client.build_image(&build_opts).await;
    let elapsed_str = format_elapsed(start.elapsed());
    match &result {
        Ok(_) => ctx.progress.println(&format!(
            "  \x1b[32m✓\x1b[0m Built features layer{elapsed_str}"
        )),
        Err(e) => ctx
            .progress
            .println(&format!("  \x1b[31m✗\x1b[0m Building features layer: {e}")),
    }
    result?;
    Ok(features_image)
}

/// Inputs for [`ensure_image`].
pub struct EnsureImageInput<'a> {
    pub client: &'a dyn ContainerBackend,
    pub config: &'a serde_json::Value,
    pub workspace_root: &'a Path,
    pub config_name: Option<&'a str>,
    pub config_path: &'a Path,
    pub no_cache: bool,
    pub pull_policy: Option<&'a str>,
    /// `BuildKit` secrets forwarded to every `docker build` as `--secret` flags.
    pub secrets: &'a [BuildSecret],
    pub progress: &'a ProgressSender,
}

/// Ensure the dev container image exists (pull or build), including features layer.
///
/// When `no_cache` is true, `--no-cache` and `--pull` are passed to the base
/// image build (but only `--no-cache` for the features layer, since its FROM
/// image is local-only) and image-based configs force re-pull.
///
/// When `pull_policy` is `Some("always")`, the base image is always re-pulled
/// (for image-based configs) or `--pull` is added to the build command (for
/// Dockerfile-based configs), even when a cached image exists locally.
///
/// # Errors
///
/// Returns an error if the image pull/build or feature resolution fails.
pub async fn ensure_image(
    input: &EnsureImageInput<'_>,
) -> Result<
    (String, Option<ResolvedFeatures>, ImageDetails),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let has_features = input
        .config
        .get("features")
        .and_then(|v| v.as_object())
        .is_some_and(|obj| !obj.is_empty());

    let base_image_tag = resolve_base_image(input, has_features).await?;

    let base_image_details = input.client.inspect_image_details(&base_image_tag).await?;

    if !has_features {
        return Ok((base_image_tag, None, base_image_details));
    }

    let features_input = FeaturesBuildInput {
        client: input.client,
        config: input.config,
        config_path: input.config_path,
        workspace_root: input.workspace_root,
        config_name: input.config_name,
        base_image_tag: &base_image_tag,
        base_image_details: &base_image_details,
        no_cache: input.no_cache,
        progress: input.progress,
    };
    let (features_image, resolved) = resolve_and_build_features(&features_input).await?;

    Ok((features_image, Some(resolved), base_image_details))
}

/// Pull or build the base image and return its tag.
///
/// When `will_build_features` is false and the config has a Dockerfile build,
/// the devcontainer lifecycle metadata is embedded as a Docker label so that
/// prebuilt images carry their lifecycle commands. This is skipped when
/// features will be layered on top, because the features build produces its
/// own metadata label and including it here would cause duplication.
async fn resolve_base_image(
    input: &EnsureImageInput<'_>,
    will_build_features: bool,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let force_pull = input.pull_policy == Some("always");
    let never_pull = input.pull_policy == Some("never");
    if let Some(image) = input.config.get("image").and_then(|v| v.as_str()) {
        let exists = input.client.image_exists(image).await?;
        if never_pull {
            if !exists {
                return Err(format!(
                    "image {image} not found locally and --pull never was specified"
                )
                .into());
            }
        } else if input.no_cache || force_pull || !exists {
            let step = input.progress.step("Pulling base image...");
            input.client.pull_image(image).await?;
            step.finish();
        }
        Ok(image.to_string())
    } else if let Some(build) = input.config.get("build").and_then(|v| v.as_object()) {
        let img_name = image_name(input.workspace_root, input.config_name);
        let mut build_opts = parse_build_options(
            build,
            &img_name,
            input.workspace_root,
            input.no_cache,
            input.pull_policy,
        );
        build_opts.secrets = input.secrets.to_vec();

        if !will_build_features {
            let metadata_label = cella_features::generate_metadata_label(&[], input.config, None);
            build_opts
                .options
                .push(format!("--label=devcontainer.metadata={metadata_label}"));
        }

        let start = std::time::Instant::now();
        input
            .progress
            .println("  \x1b[36m▸\x1b[0m Building Dockerfile...");
        let result = input.client.build_image(&build_opts).await;
        let elapsed_str = format_elapsed(start.elapsed());
        match &result {
            Ok(_) => {
                input
                    .progress
                    .println(&format!("  \x1b[32m✓\x1b[0m Built Dockerfile{elapsed_str}"));
            }
            Err(e) => {
                input
                    .progress
                    .println(&format!("  \x1b[31m✗\x1b[0m Building Dockerfile: {e}"));
            }
        }
        result?;
        Ok(img_name)
    } else {
        Err("devcontainer.json must specify either 'image' or 'build'".into())
    }
}

/// Inputs for resolving and building the features layer image.
struct FeaturesBuildInput<'a> {
    client: &'a dyn ContainerBackend,
    config: &'a serde_json::Value,
    config_path: &'a Path,
    workspace_root: &'a Path,
    config_name: Option<&'a str>,
    base_image_tag: &'a str,
    base_image_details: &'a ImageDetails,
    no_cache: bool,
    progress: &'a ProgressSender,
}

/// Resolve features and build the features layer image.
async fn resolve_and_build_features(
    input: &FeaturesBuildInput<'_>,
) -> Result<(String, ResolvedFeatures), Box<dyn std::error::Error + Send + Sync>> {
    info!("Resolving devcontainer features...");
    let backend_platform = input
        .client
        .detect_platform()
        .await
        .map_err(|e| format!("platform detection failed: {e}"))?;
    let platform =
        cella_features::oci::detect_platform(&backend_platform.os, &backend_platform.arch);
    let cache = cella_features::FeatureCache::new();

    let resolved = cella_features::resolve_features(
        input.config,
        input.config_path,
        &platform,
        &cache,
        &cella_features::BaseImageContext {
            base_image: input.base_image_tag,
            image_user: &input.base_image_details.user,
            metadata: input.base_image_details.metadata.as_deref(),
        },
        false, // non-compose: build context IS the features dir, bare COPY works
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
/// Inject host proxy env vars into build args so Dockerfile RUN steps
/// work behind corporate proxies.
///
/// Docker automatically recognizes `HTTP_PROXY`, `HTTPS_PROXY`, and
/// `NO_PROXY` as predefined build args without requiring `ARG` declarations.
pub fn inject_proxy_build_args(
    opts: &mut BuildOptions,
    proxy: &cella_network::config::ProxyConfig,
) {
    let Some(proxy_vars) = cella_network::proxy_env::ProxyEnvVars::detect(proxy) else {
        return;
    };
    if !proxy_vars.has_proxy() {
        return;
    }
    for (key, value) in proxy_vars.to_build_args() {
        opts.args.entry(key).or_insert(value);
    }
    tracing::debug!("Injected proxy build args into Docker build");
}

pub fn parse_build_options(
    build: &serde_json::Map<String, serde_json::Value>,
    img_name: &str,
    workspace_root: &Path,
    no_cache: bool,
    pull_policy: Option<&str>,
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
    } else if pull_policy == Some("always") {
        options.push("--pull".to_string());
    }
    // Note: `--pull never` for Dockerfile builds needs no special handling.
    // Docker's `build` command does not pull FROM images unless `--pull` is
    // explicitly passed, so omitting `--pull` is effectively "never" for the
    // base build.  (buildx also has no `--pull=false`; omission is correct.)

    BuildOptions {
        image_name: img_name.to_string(),
        context_path,
        dockerfile,
        args,
        target,
        cache_from,
        options,
        secrets: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── parse_build_options ──────────────────────────────────────────────

    #[test]
    fn parse_build_options_no_cache_adds_flags() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "test:latest", Path::new("/ws"), true, None);
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_without_no_cache() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "test:latest", Path::new("/ws"), false, None);
        assert!(!opts.options.contains(&"--no-cache".to_string()));
        assert!(!opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_preserves_existing_options() {
        let build: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{"dockerfile": "Dockerfile", "context": ".", "options": ["--squash"]}"#,
        )
        .unwrap();
        let opts = parse_build_options(&build, "test:latest", Path::new("/ws"), true, None);
        assert!(opts.options.contains(&"--squash".to_string()));
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_defaults() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r"{}").unwrap();
        let opts = parse_build_options(&build, "img:tag", Path::new("/ws"), false, None);
        assert_eq!(opts.image_name, "img:tag");
        assert_eq!(opts.dockerfile, "Dockerfile");
        assert_eq!(opts.context_path, Path::new("/ws/.devcontainer/."));
        assert!(opts.args.is_empty());
        assert!(opts.target.is_none());
        assert!(opts.cache_from.is_empty());
        assert!(opts.options.is_empty());
    }

    #[test]
    fn parse_build_options_custom_dockerfile() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile.dev"}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.dockerfile, "Dockerfile.dev");
    }

    #[test]
    fn parse_build_options_absolute_context() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"context": "/absolute/path"}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.context_path, Path::new("/absolute/path"));
    }

    #[test]
    fn parse_build_options_relative_context() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"context": "../"}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.context_path, Path::new("/ws/.devcontainer/../"));
    }

    #[test]
    fn parse_build_options_with_args() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"args": {"NODE_VERSION": "18", "DEBUG": "true"}}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.args.get("NODE_VERSION").unwrap(), "18");
        assert_eq!(opts.args.get("DEBUG").unwrap(), "true");
    }

    #[test]
    fn parse_build_options_with_target() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"target": "development"}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.target.as_deref(), Some("development"));
    }

    #[test]
    fn parse_build_options_with_cache_from() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"cacheFrom": ["img:cache", "img:latest"]}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.cache_from, vec!["img:cache", "img:latest"]);
    }

    #[test]
    fn parse_build_options_args_non_string_value_becomes_empty() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"args": {"NUM": 42}}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None);
        assert_eq!(opts.args.get("NUM").unwrap(), "");
    }

    #[test]
    fn parse_build_options_pull_always_adds_pull_flag() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, Some("always"));
        assert!(opts.options.contains(&"--pull".to_string()));
        assert!(!opts.options.contains(&"--no-cache".to_string()));
    }

    #[test]
    fn parse_build_options_pull_missing_does_not_add_pull_flag() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, Some("missing"));
        assert!(!opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_no_cache_takes_priority_over_pull_policy() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(&build, "img", Path::new("/ws"), true, Some("always"));
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
        // --pull should only appear once (from no_cache path, not duplicated)
        assert_eq!(opts.options.iter().filter(|o| *o == "--pull").count(), 1);
    }

    // ── compute_features_digest ──────────────────────────────────────────

    #[test]
    fn compute_features_digest_deterministic() {
        let config = json!({"features": {"ghcr.io/devcontainers/features/node:1": {}}});
        let d1 = compute_features_digest(&config);
        let d2 = compute_features_digest(&config);
        assert_eq!(d1, d2);
    }

    #[test]
    fn compute_features_digest_no_features_key() {
        let config = json!({"image": "ubuntu"});
        let d = compute_features_digest(&config);
        // Should hash the null value, still a valid hex string
        assert_eq!(d.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn compute_features_digest_different_features_differ() {
        let config_a = json!({"features": {"a": {}}});
        let config_b = json!({"features": {"b": {}}});
        assert_ne!(
            compute_features_digest(&config_a),
            compute_features_digest(&config_b)
        );
    }

    #[test]
    fn compute_features_digest_empty_features() {
        let config = json!({"features": {}});
        let d = compute_features_digest(&config);
        assert_eq!(d.len(), 64);
    }

    // ── inject_proxy_build_args ──────────────────────────────────────────

    #[test]
    fn inject_proxy_build_args_no_proxy_config() {
        let proxy = cella_network::config::ProxyConfig::default();
        let mut opts = BuildOptions {
            image_name: "test".to_string(),
            context_path: PathBuf::from("."),
            dockerfile: "Dockerfile".to_string(),
            args: HashMap::new(),
            target: None,
            cache_from: vec![],
            options: vec![],
            secrets: vec![],
        };
        inject_proxy_build_args(&mut opts, &proxy);
        // With no proxy env vars set and default config, args should remain empty
        assert!(opts.args.is_empty() || opts.args.values().all(|v| !v.is_empty()));
    }

    #[test]
    fn inject_proxy_build_args_does_not_overwrite_existing() {
        let proxy = cella_network::config::ProxyConfig::default();
        let mut opts = BuildOptions {
            image_name: "test".to_string(),
            context_path: PathBuf::from("."),
            dockerfile: "Dockerfile".to_string(),
            args: HashMap::from([("HTTP_PROXY".to_string(), "http://custom:1234".to_string())]),
            target: None,
            cache_from: vec![],
            options: vec![],
            secrets: vec![],
        };
        inject_proxy_build_args(&mut opts, &proxy);
        // Existing value must not be overwritten
        assert_eq!(opts.args.get("HTTP_PROXY").unwrap(), "http://custom:1234");
    }
}
