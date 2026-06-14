//! Container image resolution: pull, build, and features layer.
//!
//! Moved from `cella-cli/src/commands/image.rs`. Uses [`ProgressSender`]
//! instead of the indicatif-coupled `Progress` type.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::info;

use cella_backend::{
    BuildOptions, BuildSecret, ContainerBackend, ImageDetails, compute_features_digest, image_name,
    image_name_for_worktree, image_name_with_features,
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
    /// Toolchain inputs only (docker binary + `BuildKit` decision). The
    /// features layer never inherits CLI cache I/O — its FROM image is a
    /// cella-internal base no external registry cache would match.
    pub build_tuning: crate::config::BuildTuning<'a>,
    /// buildx output spec (`--output`) for the FINAL image. When features are
    /// layered, the features image *is* the final image, so the export spec
    /// belongs here — never on the base build, which must stay loadable for the
    /// features `FROM`.
    pub output: Option<&'a str>,
    /// Image labels (`--label key=value`) for the FINAL image. Same placement
    /// rule as `output`: when features are layered the features image is the
    /// final image, so the labels are baked here. Empty slice = no labels.
    pub labels: &'a [String],
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
        cache_to: None,
        options,
        secrets: vec![],
        use_buildkit: ctx.build_tuning.use_buildkit,
        docker_path: ctx.build_tuning.docker_path.map(str::to_string),
        platform: ctx.build_tuning.platform.map(str::to_string),
        // Features layer is the final image when features are present, so the
        // buildx `--output` export spec is applied here.
        output: ctx.output.map(str::to_string),
        // Likewise the final image, so user `--label`s are baked here (the base
        // build stays unlabeled — it is an internal FROM, not the user's image).
        labels: ctx.labels.to_vec(),
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
    /// Build/backend tuning. Toolchain inputs (`docker_path`, `use_buildkit`)
    /// apply to every build site; cache I/O applies to the base build only.
    pub build_tuning: crate::config::BuildTuning<'a>,
    /// buildx output spec (`--output <spec>`, e.g. `type=local,dest=…`). Applied
    /// to exactly ONE build — the final image: the base build when there are no
    /// features, or the features layer when there are. It is never applied to a
    /// base build that a features layer will `FROM`, since an export spec does
    /// not load an image into the docker store. `None` keeps the default
    /// `--load` everywhere (the `up` path, which must run the container).
    pub output: Option<&'a str>,
    /// Image labels (`--label key=value`, the `cella build --label` flag) to
    /// bake into the FINAL image. Same placement rule as [`Self::output`]:
    /// applied to exactly one build — the base build when there are no features,
    /// the features layer when there are — never to a base build that a features
    /// layer will `FROM`. Unlike `output`, labels work on both the classic and buildx
    /// builders. An empty slice bakes nothing (the `up` path, which never labels).
    ///
    /// A non-empty slice on a bare `image:` config with no features has no build
    /// to attach to (cella does not wrap a pulled image in a Dockerfile), so
    /// [`ensure_image`] returns an error rather than silently dropping the
    /// labels — detected via [`image_labels_have_no_target`].
    pub labels: &'a [String],
    /// `--omit-config-remote-env-from-metadata`: strip `remoteEnv` from the
    /// generated `devcontainer.metadata` label baked into the built image.
    pub omit_remote_env_from_metadata: bool,
    /// Lockfile policy for feature resolution.
    pub lockfile_policy: cella_features::LockfilePolicy,
    pub progress: &'a ProgressSender,
}

/// The buildx output spec for the *base* image build.
///
/// `--output` belongs on exactly one build — the final image. When a features
/// layer will be built on top (`will_build_features`), that layer is the final
/// image and carries `--output`; the base build must NOT, because an export
/// spec (e.g. `type=local`) does not load an image into the docker store, so a
/// features `FROM <base>` would fail. Returns `None` in that case; otherwise
/// the base build is the final image and gets the spec.
const fn base_output(will_build_features: bool, output: Option<&str>) -> Option<&str> {
    if will_build_features { None } else { output }
}

/// The image labels (`--label`) for the *base* image build.
///
/// Same placement rule as [`base_output`]: labels belong on exactly one build —
/// the final image. When a features layer follows (`will_build_features`), it is
/// the final image and carries the labels; the base build gets none, since it is
/// an internal `FROM` target, not the image the user keeps. Otherwise the base
/// build is the final image and gets all the labels. Unlike `--output`, labels
/// do not affect whether the image loads, so there is no skip-inspect companion.
fn base_labels(will_build_features: bool, labels: &[String]) -> Vec<String> {
    if will_build_features {
        Vec::new()
    } else {
        labels.to_vec()
    }
}

/// Whether `--label`s were requested but have no build to attach to.
///
/// `--label` bakes a label via a `docker build --label`, so it needs a build.
/// A bare `image:` config with no features runs no build — cella uses the pulled
/// image as-is and does not wrap it in a Dockerfile — so requested labels would
/// be silently dropped. `ensure_image` calls this in the image branch (where
/// `will_build_features` is false iff there are no features) and errors instead.
/// With features, the labels ride the features layer, so this is false. This is
/// consistent with cella not stamping `devcontainer.metadata` on pulled images.
const fn image_labels_have_no_target(will_build_features: bool, has_labels: bool) -> bool {
    has_labels && !will_build_features
}

/// Whether to skip inspecting the base image after the build.
///
/// A non-loading `--output` export with no features builds the image but does
/// NOT load it into the local store, so a follow-up `inspect_image_details`
/// would fail right after a successful export. In that one case the inspect is
/// skipped (the no-features build path discards these details, and `up` never
/// exports). With features, the base build still `--load`s (the export rides the
/// features layer), so the base remains inspectable.
const fn skip_base_inspect(has_features: bool, output: Option<&str>) -> bool {
    !has_features && output.is_some()
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

    // A non-loading `--output` export (no features) builds the image but does
    // NOT load it into the local store, so inspecting it would fail right after a
    // successful export. `cella build` discards these details on the no-features
    // path and `up` never exports, so return empty details instead of failing.
    if skip_base_inspect(has_features, input.output) {
        return Ok((base_image_tag, None, ImageDetails::default()));
    }

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
        build_tuning: input.build_tuning,
        output: input.output,
        labels: input.labels,
        omit_remote_env_from_metadata: input.omit_remote_env_from_metadata,
        lockfile_policy: input.lockfile_policy,
        progress: input.progress,
    };
    let (features_image, resolved) = resolve_and_build_features(&features_input).await?;

    Ok((features_image, Some(resolved), base_image_details))
}

/// Compute image name, using parent repo for worktrees to share the cache.
///
/// Hashes the full build-relevant config (build, image, features, Dockerfile)
/// so that worktrees with different configs get distinct image tags even when
/// sharing the same parent repo.
fn resolve_image_name(
    workspace_root: &Path,
    config_name: Option<&str>,
    config: &serde_json::Value,
) -> String {
    cella_git::parent_git_dir(workspace_root).map_or_else(
        || image_name(workspace_root, config_name),
        |parent_git| {
            let parent_repo = parent_git.parent().unwrap_or(&parent_git).to_path_buf();
            let config_hash = build_config_digest(config);
            image_name_for_worktree(&parent_repo, config_name, &config_hash)
        },
    )
}

/// Hash the build-relevant parts of a devcontainer config.
///
/// Includes `build`, `image`, `dockerFile`, `dockerComposeFile`, and `features`
/// so that any change to the image definition produces a different digest.
fn build_config_digest(config: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for key in [
        "build",
        "image",
        "dockerFile",
        "dockerComposeFile",
        "features",
    ] {
        if let Some(v) = config.get(key) {
            let canonical = serde_json::to_string(v).unwrap_or_default();
            hasher.update(key.as_bytes());
            hasher.update(canonical.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
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
        // A bare `image:` config with no features runs no build, so `--label`
        // has nothing to attach to (cella uses the pulled image as-is rather
        // than wrapping it in a Dockerfile). Fail loudly *before* the pull
        // instead of silently dropping the labels. With features the labels ride
        // the features layer, so `will_build_features` gates this off.
        if image_labels_have_no_target(will_build_features, !input.labels.is_empty()) {
            return Err(
                "--label requires a build (a Dockerfile, build:, or features); \
                        a bare image: config is used as-is and cannot be labeled."
                    .into(),
            );
        }
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
        // Image-only config: there is no build here. With features, the export
        // spec rides the features layer (handled by the caller); with no
        // features there is no build at all, so a `--output` has nothing to
        // attach to and is a no-op. This matches the official CLI, whose
        // `extendImage` is a passthrough for an image-only, zero-feature config
        // and never runs a buildx `--output` build.
        Ok(image.to_string())
    } else if let Some(build) = effective_build_config(input.config) {
        let img_name = resolve_image_name(input.workspace_root, input.config_name, input.config);
        let mut build_opts = parse_build_options(
            &build,
            &img_name,
            input.workspace_root,
            input.no_cache,
            input.pull_policy,
            input.build_tuning,
        );
        build_opts.secrets = input.secrets.to_vec();
        // `--output` lands on the base build only when it is the final image
        // (no features layer to follow). With features, the export spec goes on
        // the features layer instead so the base stays loadable for its `FROM`.
        build_opts.output = base_output(will_build_features, input.output).map(str::to_string);
        // User `--label`s land on the base build only when it is the final image;
        // with features they move to the features layer (the final image).
        build_opts.labels = base_labels(will_build_features, input.labels);

        if !will_build_features {
            let metadata_label = cella_features::generate_metadata_label(
                &[],
                input.config,
                None,
                input.omit_remote_env_from_metadata,
            );
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
        Err("devcontainer.json must specify 'image', 'build', or 'dockerFile'".into())
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
    build_tuning: crate::config::BuildTuning<'a>,
    /// buildx output spec for the features layer (the final image when features
    /// are present). Threaded straight through from [`EnsureImageInput::output`].
    output: Option<&'a str>,
    /// Image labels for the features layer (the final image when features are
    /// present). Threaded straight through from [`EnsureImageInput::labels`].
    labels: &'a [String],
    omit_remote_env_from_metadata: bool,
    lockfile_policy: cella_features::LockfilePolicy,
    progress: &'a ProgressSender,
}

/// Parse a `--platform` string (e.g. `linux/amd64`) into `(os, arch)`.
///
/// Returns `None` when the string is malformed (no `/` separator or empty
/// component). The arch component is passed through as-is; normalisation to
/// Go/OCI conventions (`amd64`, `arm64`) is done by
/// [`cella_features::oci::detect_platform`].
fn parse_platform_str(platform: &str) -> Option<(&str, &str)> {
    let (os, arch) = platform.split_once('/')?;
    if os.is_empty() || arch.is_empty() {
        return None;
    }
    Some((os, arch))
}

/// Resolve features and build the features layer image.
async fn resolve_and_build_features(
    input: &FeaturesBuildInput<'_>,
) -> Result<(String, ResolvedFeatures), Box<dyn std::error::Error + Send + Sync>> {
    info!("Resolving devcontainer features...");

    // When `--platform` is set (cross-arch build), resolve feature artifacts
    // for the *requested* platform rather than the Docker engine's host arch.
    // Without this, host-arch feature payloads would be baked into a foreign-
    // arch image, breaking or silently mis-installing platform-specific scripts.
    let platform = if let Some(plat_str) = input.build_tuning.platform
        && let Some((os, arch)) = parse_platform_str(plat_str)
    {
        cella_features::oci::detect_platform(os, arch)
    } else {
        let backend_platform = input
            .client
            .detect_platform()
            .await
            .map_err(|e| format!("platform detection failed: {e}"))?;
        cella_features::oci::detect_platform(&backend_platform.os, &backend_platform.arch)
    };
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
            omit_remote_env: input.omit_remote_env_from_metadata,
        },
        false, // non-compose: build context IS the features dir, bare COPY works
        input.lockfile_policy,
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
        build_tuning: input.build_tuning.toolchain(),
        output: input.output,
        labels: input.labels,
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

/// Build a normalized `build` object for a Dockerfile dev container, returning
/// `None` when the config is not Dockerfile-based.
///
/// Supports the legacy top-level `dockerFile` (+ top-level `context`) form
/// alongside the modern `build.dockerfile`/`build.context` form, mirroring the
/// official `isDockerFileConfig`/`getDockerfile`: a top-level `dockerFile` wins
/// over `build.dockerfile`, and `build.context` is preferred over a legacy
/// top-level `context`. The `args`/`target`/`options`/`cacheFrom` fields live
/// only under `build` in both forms.
fn effective_build_config(
    config: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let build = config.get("build").and_then(|v| v.as_object());
    let top_dockerfile = config.get("dockerFile").and_then(|v| v.as_str());
    let build_dockerfile = build
        .and_then(|b| b.get("dockerfile"))
        .and_then(|v| v.as_str());

    // Not a Dockerfile-based config: no top-level `dockerFile`, no `build` object, and no
    // `build.dockerfile`. A `build` object without an explicit `dockerfile` is valid — it
    // defaults to "Dockerfile" in `parse_build_options`.
    if top_dockerfile.is_none() && build.is_none() && build_dockerfile.is_none() {
        return None;
    }

    let mut map = build.cloned().unwrap_or_default();

    // Legacy top-level `dockerFile` takes precedence (official `getDockerfile`).
    if let Some(df) = top_dockerfile {
        map.insert(
            "dockerfile".to_string(),
            serde_json::Value::String(df.to_string()),
        );
    }
    // Fall back to the legacy top-level `context` when `build.context` is absent.
    if !map.contains_key("context")
        && let Some(ctx) = config.get("context").and_then(|v| v.as_str())
    {
        map.insert(
            "context".to_string(),
            serde_json::Value::String(ctx.to_string()),
        );
    }

    Some(map)
}

pub fn parse_build_options(
    build: &serde_json::Map<String, serde_json::Value>,
    img_name: &str,
    workspace_root: &Path,
    no_cache: bool,
    pull_policy: Option<&str>,
    build_tuning: crate::config::BuildTuning<'_>,
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

    // `cacheFrom` may be a single string OR an array of strings (spec allows
    // both); the string form must not be dropped.
    let config_cache_from: Vec<String> = match build.get("cacheFrom") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };

    // CLI `--cache-from` is prepended before config `cacheFrom` (matching the
    // official ordering); both are dropped entirely under `--no-cache`.
    let cache_from: Vec<String> = if no_cache {
        Vec::new()
    } else {
        build_tuning
            .cli_cache_from
            .iter()
            .cloned()
            .chain(config_cache_from)
            .collect()
    };

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
        // `--cache-to` applies to the base Dockerfile build only.
        cache_to: build_tuning.cache_to.map(str::to_string),
        options,
        secrets: vec![],
        use_buildkit: build_tuning.use_buildkit,
        docker_path: build_tuning.docker_path.map(str::to_string),
        platform: build_tuning.platform.map(str::to_string),
        // `--output` is not part of BuildTuning (it must reach only the final
        // build, not every site): the caller sets it via `base_output` when this
        // base build is the final image.
        output: None,
        // Same as `output`: the caller sets labels via `base_labels` only when
        // this base build is the final image (no features layer to follow).
        labels: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BuildTuning;
    use serde_json::json;

    // ── parse_build_options ──────────────────────────────────────────────

    #[test]
    fn parse_build_options_no_cache_adds_flags() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "test:latest",
            Path::new("/ws"),
            true,
            None,
            BuildTuning::default(),
        );
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_without_no_cache() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "test:latest",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert!(!opts.options.contains(&"--no-cache".to_string()));
        assert!(!opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_preserves_existing_options() {
        let build: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{"dockerfile": "Dockerfile", "context": ".", "options": ["--squash"]}"#,
        )
        .unwrap();
        let opts = parse_build_options(
            &build,
            "test:latest",
            Path::new("/ws"),
            true,
            None,
            BuildTuning::default(),
        );
        assert!(opts.options.contains(&"--squash".to_string()));
        assert!(opts.options.contains(&"--no-cache".to_string()));
        assert!(opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_defaults() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r"{}").unwrap();
        let opts = parse_build_options(
            &build,
            "img:tag",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
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
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.dockerfile, "Dockerfile.dev");
    }

    #[test]
    fn parse_build_options_absolute_context() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"context": "/absolute/path"}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.context_path, Path::new("/absolute/path"));
    }

    #[test]
    fn parse_build_options_relative_context() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"context": "../"}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.context_path, Path::new("/ws/.devcontainer/../"));
    }

    #[test]
    fn parse_build_options_with_args() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"args": {"NODE_VERSION": "18", "DEBUG": "true"}}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.args.get("NODE_VERSION").unwrap(), "18");
        assert_eq!(opts.args.get("DEBUG").unwrap(), "true");
    }

    #[test]
    fn parse_build_options_with_target() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"target": "development"}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.target.as_deref(), Some("development"));
    }

    #[test]
    fn parse_build_options_with_cache_from() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"cacheFrom": ["img:cache", "img:latest"]}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.cache_from, vec!["img:cache", "img:latest"]);
    }

    #[test]
    fn parse_build_options_cli_cache_from_prepended_before_config() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"cacheFrom": ["cfg:1"]}"#).unwrap();
        let cli = vec!["cli:1".to_string(), "cli:2".to_string()];
        let tuning = BuildTuning {
            cli_cache_from: &cli,
            use_buildkit: true,
            ..Default::default()
        };
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None, tuning);
        assert_eq!(opts.cache_from, vec!["cli:1", "cli:2", "cfg:1"]);
    }

    #[test]
    fn parse_build_options_no_cache_drops_all_cache_from() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"cacheFrom": ["cfg:1"]}"#).unwrap();
        let cli = vec!["cli:1".to_string()];
        let tuning = BuildTuning {
            cli_cache_from: &cli,
            ..Default::default()
        };
        let opts = parse_build_options(&build, "img", Path::new("/ws"), true, None, tuning);
        assert!(opts.cache_from.is_empty());
    }

    #[test]
    fn parse_build_options_threads_cache_to_and_toolchain() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r"{}").unwrap();
        let tuning = BuildTuning {
            docker_path: Some("/usr/local/bin/docker"),
            use_buildkit: true,
            cache_to: Some("type=registry,ref=r"),
            cli_cache_from: &[],
            platform: None,
        };
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None, tuning);
        assert_eq!(opts.cache_to.as_deref(), Some("type=registry,ref=r"));
        assert!(opts.use_buildkit);
        assert_eq!(opts.docker_path.as_deref(), Some("/usr/local/bin/docker"));
    }

    #[test]
    fn parse_build_options_args_non_string_value_becomes_empty() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"args": {"NUM": 42}}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.args.get("NUM").unwrap(), "");
    }

    #[test]
    fn parse_build_options_pull_always_adds_pull_flag() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            Some("always"),
            BuildTuning::default(),
        );
        assert!(opts.options.contains(&"--pull".to_string()));
        assert!(!opts.options.contains(&"--no-cache".to_string()));
    }

    #[test]
    fn parse_build_options_pull_missing_does_not_add_pull_flag() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            Some("missing"),
            BuildTuning::default(),
        );
        assert!(!opts.options.contains(&"--pull".to_string()));
    }

    #[test]
    fn parse_build_options_no_cache_takes_priority_over_pull_policy() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "context": "."}"#).unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            true,
            Some("always"),
            BuildTuning::default(),
        );
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
            ..Default::default()
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
            ..Default::default()
        };
        inject_proxy_build_args(&mut opts, &proxy);
        // Existing value must not be overwritten
        assert_eq!(opts.args.get("HTTP_PROXY").unwrap(), "http://custom:1234");
    }

    #[test]
    fn build_config_digest_differs_for_different_images() {
        let config_a = serde_json::json!({ "image": "node:20" });
        let config_b = serde_json::json!({ "image": "python:3.12" });
        assert_ne!(
            build_config_digest(&config_a),
            build_config_digest(&config_b)
        );
    }

    #[test]
    fn build_config_digest_same_config_is_deterministic() {
        let config = serde_json::json!({ "build": { "dockerfile": "Dockerfile" } });
        assert_eq!(build_config_digest(&config), build_config_digest(&config));
    }

    #[test]
    fn build_config_digest_includes_features() {
        let config_a = serde_json::json!({ "image": "node:20", "features": {} });
        let config_b = serde_json::json!({ "image": "node:20", "features": { "ghcr.io/devcontainers/features/node:1": {} } });
        assert_ne!(
            build_config_digest(&config_a),
            build_config_digest(&config_b)
        );
    }

    #[test]
    fn parse_build_options_platform_is_forwarded() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r"{}").unwrap();
        let tuning = BuildTuning {
            platform: Some("linux/amd64"),
            ..Default::default()
        };
        let opts = parse_build_options(&build, "img", Path::new("/ws"), false, None, tuning);
        assert_eq!(opts.platform.as_deref(), Some("linux/amd64"));
    }

    #[test]
    fn parse_build_options_platform_none_by_default() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r"{}").unwrap();
        let opts = parse_build_options(
            &build,
            "img",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert!(opts.platform.is_none());
    }

    #[test]
    fn cache_from_string_form_is_kept() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "cacheFrom": "reg/img:cache"}"#)
                .unwrap();
        let opts = parse_build_options(
            &build,
            "t:latest",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.cache_from, vec!["reg/img:cache".to_string()]);
    }

    #[test]
    fn cache_from_array_form_is_kept() {
        let build: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"dockerfile": "Dockerfile", "cacheFrom": ["a", "b"]}"#)
                .unwrap();
        let opts = parse_build_options(
            &build,
            "t:latest",
            Path::new("/ws"),
            false,
            None,
            BuildTuning::default(),
        );
        assert_eq!(opts.cache_from, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn effective_build_config_legacy_top_level_dockerfile() {
        let cfg = serde_json::json!({ "dockerFile": "Dockerfile", "context": ".." });
        let build = effective_build_config(&cfg).expect("legacy dockerFile is a build config");
        assert_eq!(build.get("dockerfile").unwrap(), "Dockerfile");
        // Legacy top-level context falls back when build.context is absent.
        assert_eq!(build.get("context").unwrap(), "..");
    }

    #[test]
    fn effective_build_config_modern_and_image() {
        assert!(
            effective_build_config(&serde_json::json!({ "build": { "dockerfile": "Dockerfile" } }))
                .is_some()
        );
        // Pure image config is not a Dockerfile build.
        assert!(effective_build_config(&serde_json::json!({ "image": "node:20" })).is_none());
    }

    #[test]
    fn effective_build_config_build_object_without_dockerfile_is_valid() {
        // A `build` object with no explicit `dockerfile` is a valid Dockerfile config —
        // `parse_build_options` defaults `dockerfile` to "Dockerfile".
        let cfg = serde_json::json!({ "build": { "args": { "FOO": "bar" } } });
        let build = effective_build_config(&cfg)
            .expect("build object without dockerfile should return Some");
        assert_eq!(
            build.get("args").unwrap(),
            &serde_json::json!({ "FOO": "bar" })
        );
        // No dockerfile key injected — `parse_build_options` will apply the default.
        assert!(build.get("dockerfile").is_none());
    }

    #[test]
    fn effective_build_config_top_level_dockerfile_wins() {
        let cfg = serde_json::json!({
            "dockerFile": "Legacy.Dockerfile",
            "build": { "dockerfile": "Modern.Dockerfile", "target": "dev" }
        });
        let build = effective_build_config(&cfg).unwrap();
        assert_eq!(build.get("dockerfile").unwrap(), "Legacy.Dockerfile");
        // build-only fields are preserved.
        assert_eq!(build.get("target").unwrap(), "dev");
    }

    // ── parse_platform_str ──────────────────────────────────────────────────

    #[test]
    fn parse_platform_str_standard_forms() {
        assert_eq!(parse_platform_str("linux/amd64"), Some(("linux", "amd64")));
        assert_eq!(parse_platform_str("linux/arm64"), Some(("linux", "arm64")));
        assert_eq!(
            parse_platform_str("linux/arm/v7"),
            Some(("linux", "arm/v7"))
        );
    }

    #[test]
    fn parse_platform_str_rejects_malformed() {
        assert!(parse_platform_str("linuxamd64").is_none());
        assert!(parse_platform_str("/amd64").is_none());
        assert!(parse_platform_str("linux/").is_none());
        assert!(parse_platform_str("").is_none());
    }

    // ── base_output: --output placement (final-build only) ───────────
    //
    // `ensure_image` is Docker-coupled, so the load-bearing placement rule
    // (the base build gets `--output` *only* when it is the final image) is
    // pinned here on the pure helper instead. A regression that applied the
    // export spec to a base build that a features layer will `FROM` would break
    // every featured devcontainer — these arms guard exactly that.

    #[test]
    fn base_output_suppressed_when_features_will_build() {
        // With a features layer to follow, the base build must NOT carry the
        // export spec — it has to stay loadable for the features `FROM`. The
        // spec moves to the features layer (the final image) instead.
        assert_eq!(base_output(true, Some("type=local,dest=/tmp/out")), None);
        assert_eq!(base_output(true, None), None);
    }

    #[test]
    fn base_output_applied_when_base_is_final() {
        // No features: the base build IS the final image, so it carries the
        // export spec verbatim (and stays `None` when none was requested).
        assert_eq!(
            base_output(false, Some("type=local,dest=/tmp/out")),
            Some("type=local,dest=/tmp/out")
        );
        assert_eq!(base_output(false, None), None);
    }

    // ── base_labels: --label placement (final-build only) ────────────
    //
    // Same load-bearing rule as `base_output`: user `--label`s belong on the
    // final image, so a base build that a features layer will `FROM` must stay
    // unlabeled (the labels move to the features layer). Pinned on the pure
    // helper since `resolve_base_image` is Docker-coupled.

    #[test]
    fn base_labels_suppressed_when_features_will_build() {
        // Features to follow → the base build is an internal FROM, not the final
        // image, so it gets no user labels (they ride the features layer).
        assert!(base_labels(true, &["a=1".to_string(), "b=2".to_string()]).is_empty());
        assert!(base_labels(true, &[]).is_empty());
    }

    #[test]
    fn base_labels_applied_when_base_is_final() {
        // No features: the base build IS the final image, so it carries every
        // label verbatim (and stays empty when none were requested).
        assert_eq!(
            base_labels(false, &["a=1".to_string(), "b=2".to_string()]),
            vec!["a=1".to_string(), "b=2".to_string()]
        );
        assert!(base_labels(false, &[]).is_empty());
    }

    // ── image_labels_have_no_target: bare image: + --label error ─────
    //
    // `--label` needs a build. A bare `image:` config with no features runs no
    // build (cella uses the pulled image as-is), so requested labels would be
    // silently dropped — `ensure_image` errors instead. With features the labels
    // ride the features layer, so there IS a target. This pins exactly that gate.

    #[test]
    fn image_labels_have_no_target_only_when_no_build_and_labels() {
        // Bare image: (no features) + labels requested → no target, must error.
        assert!(image_labels_have_no_target(false, true));
        // No labels requested → nothing to drop, no error regardless of build.
        assert!(!image_labels_have_no_target(false, false));
        // Features will build → the features layer is the label target; OK.
        assert!(!image_labels_have_no_target(true, true));
        assert!(!image_labels_have_no_target(true, false));
    }

    // ── skip_base_inspect: don't inspect a non-loaded export ─────────
    //
    // `ensure_image` inspects the base image after building. A non-loading
    // `--output` export (no features) leaves nothing in the local store to
    // inspect, so the inspect MUST be skipped — otherwise the command fails
    // right after a successful export. A regression that re-enabled the inspect
    // there would break every `cella build --output type=local/tar` on a
    // Dockerfile config; this pins exactly that case.

    #[test]
    fn skip_base_inspect_only_for_no_features_export() {
        // No features + a `--output` export → skip the inspect (image not loaded).
        assert!(skip_base_inspect(false, Some("type=local,dest=/tmp/out")));
        // No export → inspect normally (the build loaded the image).
        assert!(!skip_base_inspect(false, None));
        // With features the base build still loads (export rides the features
        // layer), so the base stays inspectable regardless of `--output`.
        assert!(!skip_base_inspect(true, Some("type=local,dest=/tmp/out")));
        assert!(!skip_base_inspect(true, None));
    }
}
