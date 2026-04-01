pub mod auth;
pub mod cache;
pub mod dockerfile;
mod error;
pub mod fetch;
pub mod merge;
pub mod metadata;
pub mod oci;
pub mod ordering;
pub mod reference;
pub mod types;

#[cfg(test)]
mod test_utils;

pub use cache::FeatureCache;
pub use dockerfile::{
    FEATURE_CONTENT_SOURCE, generate_builtin_env, generate_dockerfile, generate_entrypoint_script,
    generate_feature_env, generate_wrapper_script,
};
pub use error::{FeatureError, FeatureWarning};
pub use fetch::{HttpFetcher, LocalFetcher};
pub use merge::{
    ImageMetadataUserInfo, merge_features, merge_with_devcontainer, parse_image_metadata,
    validate_options,
};
pub use metadata::parse_feature_metadata;
pub use oci::{FeatureFetcher, OciFetcher};
pub use ordering::compute_install_order;
pub use reference::{FeatureRef, NormalizedRef};
pub use types::*;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parse lifecycle commands for a phase from a `devcontainer.metadata` label.
///
/// Each metadata entry with an `"id"` field uses that as the origin; entries
/// without use `"devcontainer.json"`.
pub fn lifecycle_from_metadata_label(metadata_json: &str, phase: &str) -> Vec<LifecycleEntry> {
    let entries: Vec<serde_json::Value> = serde_json::from_str(metadata_json).unwrap_or_default();
    let mut result = Vec::new();
    for entry in &entries {
        let origin = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("devcontainer.json")
            .to_string();
        if let Some(cmd) = entry.get(phase)
            && !cmd.is_null()
        {
            result.push(LifecycleEntry {
                origin,
                command: cmd.clone(),
            });
        }
    }
    result
}

use futures_util::future::join_all;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Internal intermediate type for parsed feature entries
// ---------------------------------------------------------------------------

/// Context about the base image for feature resolution.
pub struct BaseImageContext<'a> {
    /// The base image reference (e.g., stage name or `ubuntu:22.04`).
    pub base_image: &'a str,
    /// The user in the base image (e.g., `"root"`, `"vscode"`).
    pub image_user: &'a str,
    /// `devcontainer.metadata` label from the base image, if available.
    pub metadata: Option<&'a str>,
}

/// Intermediate representation of a parsed feature before ordering.
struct FeatureEntry {
    key: String,
    metadata: FeatureMetadata,
    artifact_dir: PathBuf,
    user_options: HashMap<String, serde_json::Value>,
    original_ref: String,
}

// ---------------------------------------------------------------------------
// resolve_features — main public API
// ---------------------------------------------------------------------------

/// Resolve all devcontainer features from the config into a build-ready output.
///
/// Orchestrates parsing, fetching, ordering, Dockerfile generation, and
/// metadata merging for the `features` object in `devcontainer.json`.
///
/// # Errors
///
/// Returns [`FeatureError`] when any feature reference cannot be parsed,
/// normalized, or fetched, or when metadata is invalid.
pub async fn resolve_features(
    config: &serde_json::Value,
    config_path: &Path,
    platform: &Platform,
    cache: &FeatureCache,
    base_image_ctx: &BaseImageContext<'_>,
    use_named_content_source: bool,
) -> Result<ResolvedFeatures, FeatureError> {
    // Step 1: Extract the "features" object from the config.
    let features_obj = match config.get("features").and_then(|v| v.as_object()) {
        Some(obj) if !obj.is_empty() => obj,
        _ => {
            return resolve_empty_features(config, cache, base_image_ctx.metadata);
        }
    };

    let workspace_root = config_path.parent().unwrap_or_else(|| Path::new("."));

    // Step 2: Parse each key with FeatureRef::parse and normalize.
    let parsed_entries = parse_and_normalize(features_obj, workspace_root)?;

    // Step 3: Compute config hash for the build context path.
    let config_hash = compute_config_hash(features_obj);
    let build_context = cache.build_context_path(&config_hash);

    // Step 4: Fetch all concurrently.
    let artifact_paths = fetch_all(&parsed_entries, platform, cache).await?;

    // Step 5: Parse metadata and validate options.
    let feature_entries = parse_metadata_and_validate(&parsed_entries, &artifact_paths)?;

    // Step 6: Compute install order.
    let ordered_ids = compute_order(&feature_entries, config);

    // Step 7: Assemble resolved features in install order.
    let resolved = assemble_resolved(&feature_entries, &ordered_ids);

    // Step 8: Generate Dockerfile and build context.
    let dockerfile = generate_and_write_build_context(
        &build_context,
        &resolved,
        config,
        base_image_ctx.base_image,
        base_image_ctx.image_user,
        base_image_ctx.metadata,
        use_named_content_source,
    )?;

    // Step 9: Merge feature metadata and generate label.
    let container_config = merge_all_metadata(&resolved, config, base_image_ctx.metadata);
    let metadata_label = generate_metadata_label(&resolved, config, base_image_ctx.metadata);

    debug!(
        "resolved {} features, build context at {}",
        resolved.len(),
        build_context.display()
    );

    Ok(ResolvedFeatures {
        features: resolved,
        dockerfile,
        build_context,
        container_config,
        metadata_label,
    })
}

// ---------------------------------------------------------------------------
// generate_metadata_label
// ---------------------------------------------------------------------------

/// Generate the `devcontainer.metadata` label JSON for the built image.
///
/// Produces a JSON array where:
/// - If `base_image_metadata` is provided, its entries are prepended.
/// - One object per resolved feature (with id, `containerEnv`, entrypoint,
///   mounts, customizations, and lifecycle commands).
/// - The last element is the user's `devcontainer.json` properties.
pub fn generate_metadata_label(
    features: &[ResolvedFeature],
    user_config: &serde_json::Value,
    base_image_metadata: Option<&str>,
) -> String {
    let mut entries: Vec<serde_json::Value> = Vec::new();

    // Prepend base image metadata entries if present.
    if let Some(base_meta) = base_image_metadata
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(base_meta)
    {
        match parsed {
            serde_json::Value::Array(arr) => entries.extend(arr),
            other => entries.push(other),
        }
    }

    // One entry per feature.
    for feature in features {
        entries.push(build_feature_metadata_entry(feature));
    }

    // Last element: user config properties.
    entries.push(user_config.clone());

    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------
// Internal helpers -- resolve_features sub-steps
// ---------------------------------------------------------------------------

/// Handle the case where no features are declared (empty or missing `features` object).
fn resolve_empty_features(
    config: &serde_json::Value,
    cache: &FeatureCache,
    base_image_metadata: Option<&str>,
) -> Result<ResolvedFeatures, FeatureError> {
    debug!("no features declared in devcontainer.json");
    let build_context = cache.build_context_path("empty");
    std::fs::create_dir_all(&build_context)?;
    let container_config = base_image_metadata
        .map(|m| {
            let (cfg, _) = parse_image_metadata(m);
            merge_with_devcontainer(&cfg, config)
        })
        .unwrap_or_default();
    Ok(ResolvedFeatures {
        features: Vec::new(),
        dockerfile: String::new(),
        build_context,
        container_config,
        metadata_label: generate_metadata_label(&[], config, base_image_metadata),
    })
}

/// Resolve user identities, generate the Dockerfile, and write the build context to disk.
///
/// Returns the generated Dockerfile content.
fn generate_and_write_build_context(
    build_context: &Path,
    resolved: &[ResolvedFeature],
    config: &serde_json::Value,
    base_image: &str,
    image_user: &str,
    base_image_metadata: Option<&str>,
    use_named_content_source: bool,
) -> Result<String, FeatureError> {
    // User resolution per spec: devcontainer.json > image metadata > Config.User > "root"
    let meta_user = base_image_metadata.map(|m| parse_image_metadata(m).1);
    let container_user = config
        .get("containerUser")
        .and_then(|v| v.as_str())
        .or_else(|| meta_user.as_ref().and_then(|m| m.container_user.as_deref()))
        .unwrap_or(image_user);
    let remote_user = config
        .get("remoteUser")
        .and_then(|v| v.as_str())
        .or_else(|| meta_user.as_ref().and_then(|m| m.remote_user.as_deref()))
        .unwrap_or(container_user);

    let dockerfile = generate_dockerfile(
        base_image,
        image_user,
        container_user,
        remote_user,
        resolved,
        use_named_content_source,
    );
    let entrypoint_script = generate_entrypoint_script(resolved);
    let builtin_env = generate_builtin_env(container_user, remote_user);

    prepare_build_context(
        build_context,
        resolved,
        &dockerfile,
        entrypoint_script.as_deref(),
        &builtin_env,
    )?;

    Ok(dockerfile)
}

/// Merge feature metadata with image metadata and devcontainer.json config.
fn merge_all_metadata(
    resolved: &[ResolvedFeature],
    config: &serde_json::Value,
    base_image_metadata: Option<&str>,
) -> FeatureContainerConfig {
    let image_meta_config = base_image_metadata.map(|m| parse_image_metadata(m).0);
    let feature_config = merge_features(resolved, image_meta_config);
    merge_with_devcontainer(&feature_config, config)
}

/// Parse and normalize all feature references from the config.
fn parse_and_normalize(
    features_obj: &serde_json::Map<String, serde_json::Value>,
    workspace_root: &Path,
) -> Result<Vec<(String, NormalizedRef, serde_json::Value)>, FeatureError> {
    let mut parsed_entries = Vec::new();

    for (key, options_value) in features_obj {
        let feature_ref = FeatureRef::parse(key)?;
        let (normalized, warning) = feature_ref.normalize(workspace_root)?;

        if let Some(w) = warning {
            warn!("{key}: deprecated feature reference ({w:?}), use OCI equivalent");
            // Warning logged; we don't accumulate it further since
            // warnings are advisory and already surfaced via tracing.
            let _ = w;
        }

        parsed_entries.push((key.clone(), normalized, options_value.clone()));
    }

    Ok(parsed_entries)
}

/// Fetch all features concurrently, returning their artifact paths.
async fn fetch_all(
    parsed_entries: &[(String, NormalizedRef, serde_json::Value)],
    platform: &Platform,
    cache: &FeatureCache,
) -> Result<Vec<PathBuf>, FeatureError> {
    let oci_fetcher = OciFetcher::default();
    let http_fetcher = HttpFetcher;
    let local_fetcher = LocalFetcher;

    let fetch_futures: Vec<_> = parsed_entries
        .iter()
        .map(|(key, norm_ref, _)| {
            fetch_one(
                key,
                norm_ref,
                platform,
                cache,
                &oci_fetcher,
                &http_fetcher,
                &local_fetcher,
            )
        })
        .collect();

    let fetch_results = join_all(fetch_futures).await;

    let mut artifact_paths = Vec::with_capacity(parsed_entries.len());
    for result in fetch_results {
        artifact_paths.push(result?);
    }

    Ok(artifact_paths)
}

/// Parse metadata from fetched artifacts and validate user options.
fn parse_metadata_and_validate(
    parsed_entries: &[(String, NormalizedRef, serde_json::Value)],
    artifact_paths: &[PathBuf],
) -> Result<Vec<FeatureEntry>, FeatureError> {
    let mut entries = Vec::with_capacity(parsed_entries.len());

    for (i, (key, _, options_value)) in parsed_entries.iter().enumerate() {
        let artifact_dir = &artifact_paths[i];
        let metadata_path = artifact_dir.join("devcontainer-feature.json");
        let metadata_json =
            std::fs::read_to_string(&metadata_path).map_err(|e| FeatureError::InvalidMetadata {
                feature_id: key.clone(),
                reason: format!("cannot read devcontainer-feature.json: {e}"),
            })?;

        let metadata = parse_feature_metadata(&metadata_json)?;
        let user_options = parse_user_options(options_value);

        // Advisory validation -- log warnings but don't fail.
        let warnings = validate_options(key, &user_options, &metadata.options);
        for w in &warnings {
            log_option_warning(key, w);
        }

        entries.push(FeatureEntry {
            key: key.clone(),
            metadata,
            artifact_dir: artifact_dir.clone(),
            user_options,
            original_ref: key.clone(),
        });
    }

    Ok(entries)
}

/// Compute install order from feature entries and config overrides.
fn compute_order(feature_entries: &[FeatureEntry], config: &serde_json::Value) -> Vec<String> {
    let order_input: Vec<(String, &FeatureMetadata)> = feature_entries
        .iter()
        .map(|entry| (entry.key.clone(), &entry.metadata))
        .collect();

    let override_order = config
        .get("overrideFeatureInstallOrder")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });

    let (ordered_ids, order_warnings) =
        compute_install_order(&order_input, override_order.as_deref());

    for w in &order_warnings {
        warn!("install order: {w:?}");
    }

    ordered_ids
}

/// Assemble `ResolvedFeature` structs in install order.
fn assemble_resolved(
    feature_entries: &[FeatureEntry],
    ordered_ids: &[String],
) -> Vec<ResolvedFeature> {
    let entry_map: HashMap<&str, usize> = feature_entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.key.as_str(), i))
        .collect();

    let mut resolved = Vec::with_capacity(ordered_ids.len());

    for id in ordered_ids {
        let idx = entry_map[id.as_str()];
        let entry = &feature_entries[idx];

        let has_install = entry.artifact_dir.join("install.sh").is_file();

        resolved.push(ResolvedFeature {
            id: entry.metadata.id.clone(),
            original_ref: entry.original_ref.clone(),
            metadata: entry.metadata.clone(),
            user_options: entry.user_options.clone(),
            artifact_dir: entry.artifact_dir.clone(),
            has_install_script: has_install,
        });

        debug!(
            "resolved feature {} -> {} (install_script={})",
            entry.key, entry.metadata.id, has_install
        );
    }

    resolved
}

/// Write the build context directory: feature dirs, Dockerfile, env files, wrapper scripts.
fn prepare_build_context(
    build_context: &Path,
    resolved: &[ResolvedFeature],
    dockerfile: &str,
    entrypoint_script: Option<&str>,
    builtin_env: &str,
) -> Result<(), FeatureError> {
    std::fs::create_dir_all(build_context)?;

    let has_installable = resolved.iter().any(|f| f.has_install_script);

    for feature in resolved {
        let dest = build_context.join(&feature.id);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        copy_dir_recursive(&feature.artifact_dir, &dest)?;

        // Write per-feature env and wrapper script for installable features
        if feature.has_install_script {
            std::fs::write(
                dest.join("devcontainer-features.env"),
                generate_feature_env(feature),
            )?;
            std::fs::write(
                dest.join("devcontainer-features-install.sh"),
                generate_wrapper_script(&feature.id),
            )?;
        }
    }

    std::fs::write(build_context.join("Dockerfile.features"), dockerfile)?;

    // Write builtin env file at build context root (only if features need it)
    if has_installable {
        std::fs::write(
            build_context.join("devcontainer-features.builtin.env"),
            builtin_env,
        )?;
    }

    if let Some(script) = entrypoint_script {
        std::fs::write(build_context.join("docker-init.sh"), script)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Leaf helpers
// ---------------------------------------------------------------------------

/// Build a single metadata label entry for a resolved feature.
fn build_feature_metadata_entry(feature: &ResolvedFeature) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert("id".into(), serde_json::json!(feature.id));

    if !feature.metadata.container_env.is_empty() {
        entry.insert(
            "containerEnv".into(),
            serde_json::json!(feature.metadata.container_env),
        );
    }

    if let Some(ep) = &feature.metadata.entrypoint {
        entry.insert("entrypoint".into(), serde_json::json!(ep));
    }

    if !feature.metadata.mounts.is_empty() {
        entry.insert("mounts".into(), serde_json::json!(feature.metadata.mounts));
    }

    if let Some(cust) = &feature.metadata.customizations {
        entry.insert("customizations".into(), cust.clone());
    }

    if let Some(cmd) = &feature.metadata.on_create_command {
        entry.insert("onCreateCommand".into(), cmd.clone());
    }
    if let Some(cmd) = &feature.metadata.update_content_command {
        entry.insert("updateContentCommand".into(), cmd.clone());
    }
    if let Some(cmd) = &feature.metadata.post_create_command {
        entry.insert("postCreateCommand".into(), cmd.clone());
    }
    if let Some(cmd) = &feature.metadata.post_start_command {
        entry.insert("postStartCommand".into(), cmd.clone());
    }
    if let Some(cmd) = &feature.metadata.post_attach_command {
        entry.insert("postAttachCommand".into(), cmd.clone());
    }

    serde_json::Value::Object(entry)
}

/// Log an option validation warning via tracing.
fn log_option_warning(key: &str, w: &FeatureWarning) {
    match w {
        FeatureWarning::UnknownOption { option, .. } => {
            warn!("{key}: unknown option '{option}'");
        }
        FeatureWarning::TypeMismatch {
            option,
            expected,
            got,
            ..
        } => {
            warn!("{key}: option '{option}' expected {expected}, got {got}");
        }
        FeatureWarning::InvalidEnumValue {
            option,
            value,
            allowed,
            ..
        } => {
            warn!("{key}: option '{option}' value '{value}' not in allowed set: {allowed:?}");
        }
        _ => {}
    }
}

/// Dispatch a single fetch to the appropriate fetcher based on the normalized ref.
async fn fetch_one(
    key: &str,
    norm_ref: &NormalizedRef,
    platform: &Platform,
    cache: &FeatureCache,
    oci_fetcher: &OciFetcher,
    http_fetcher: &HttpFetcher,
    local_fetcher: &LocalFetcher,
) -> Result<PathBuf, FeatureError> {
    debug!("fetching feature {key} from {norm_ref}");

    match norm_ref {
        NormalizedRef::OciTarget { .. } => oci_fetcher.fetch(norm_ref, platform, cache).await,
        NormalizedRef::HttpTarget { .. } => http_fetcher.fetch(norm_ref, platform, cache).await,
        NormalizedRef::LocalTarget { .. } => local_fetcher.fetch(norm_ref, platform, cache).await,
    }
}

/// Parse user options from a JSON value (object -> `HashMap`, anything else -> empty).
fn parse_user_options(value: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    value.as_object().map_or_else(HashMap::new, |obj| {
        obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    })
}

/// Compute a SHA-256 hash of the features object for build context deduplication.
fn compute_config_hash(features: &serde_json::Map<String, serde_json::Value>) -> String {
    let canonical = serde_json::to_string(features).unwrap_or_default();
    let hash = Sha256::digest(canonical.as_bytes());
    hex::encode(hash)[..16].to_string()
}

/// Recursively copy a directory tree from `src` to `dst`.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), FeatureError> {
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    #[cfg(feature = "integration-tests")]
    use crate::test_utils::test_platform;

    // -----------------------------------------------------------------------
    // generate_metadata_label
    // -----------------------------------------------------------------------

    #[test]
    fn metadata_label_empty_features() {
        let label = generate_metadata_label(&[], &json!({"image": "ubuntu"}), None);
        let parsed: serde_json::Value = serde_json::from_str(&label).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["image"], "ubuntu");
    }

    #[test]
    fn metadata_label_with_features() {
        let features = vec![ResolvedFeature {
            id: "node".to_string(),
            original_ref: "ghcr.io/devcontainers/features/node:1".to_string(),
            metadata: FeatureMetadata {
                id: "node".to_string(),
                container_env: HashMap::from([("NODE_PATH".to_string(), "/usr/local".to_string())]),
                entrypoint: Some("/init.sh".to_string()),
                ..Default::default()
            },
            user_options: HashMap::new(),
            artifact_dir: PathBuf::from("/tmp/features/node"),
            has_install_script: true,
        }];

        let label = generate_metadata_label(&features, &json!({}), None);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "node");
        assert_eq!(parsed[0]["entrypoint"], "/init.sh");
        assert!(parsed[0]["containerEnv"]["NODE_PATH"].is_string());
    }

    #[test]
    fn metadata_label_with_base_image_metadata() {
        let base_meta = r#"[{"id":"base","containerEnv":{"LANG":"C.UTF-8"}}]"#;
        let label = generate_metadata_label(&[], &json!({}), Some(base_meta));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();

        // base entry + user config entry
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "base");
    }

    #[test]
    fn metadata_label_base_image_non_array() {
        let base_meta = r#"{"id":"single"}"#;
        let label = generate_metadata_label(&[], &json!({}), Some(base_meta));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "single");
    }

    // -----------------------------------------------------------------------
    // parse_user_options
    // -----------------------------------------------------------------------

    #[test]
    fn parse_user_options_from_object() {
        let val = json!({"version": "18", "install_tools": true});
        let opts = parse_user_options(&val);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts["version"], json!("18"));
        assert_eq!(opts["install_tools"], json!(true));
    }

    #[test]
    fn parse_user_options_from_non_object() {
        let val = json!("just a string");
        let opts = parse_user_options(&val);
        assert!(opts.is_empty());
    }

    #[test]
    fn parse_user_options_from_empty_object() {
        let val = json!({});
        let opts = parse_user_options(&val);
        assert!(opts.is_empty());
    }

    // -----------------------------------------------------------------------
    // compute_config_hash
    // -----------------------------------------------------------------------

    #[test]
    fn config_hash_deterministic() {
        let mut map = serde_json::Map::new();
        map.insert(
            "ghcr.io/devcontainers/features/node:1".to_string(),
            json!({"version": "18"}),
        );

        let hash1 = compute_config_hash(&map);
        let hash2 = compute_config_hash(&map);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16);
    }

    #[test]
    fn config_hash_different_for_different_configs() {
        let mut map1 = serde_json::Map::new();
        map1.insert("feature-a".to_string(), json!({}));

        let mut map2 = serde_json::Map::new();
        map2.insert("feature-b".to_string(), json!({}));

        assert_ne!(compute_config_hash(&map1), compute_config_hash(&map2));
    }

    // -----------------------------------------------------------------------
    // copy_dir_recursive
    // -----------------------------------------------------------------------

    #[test]
    fn copy_dir_recursive_copies_files_and_subdirs() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let dst_path = dst.path().join("copied");

        std::fs::write(src.path().join("file.txt"), "hello").unwrap();
        std::fs::create_dir(src.path().join("subdir")).unwrap();
        std::fs::write(src.path().join("subdir").join("nested.txt"), "world").unwrap();

        copy_dir_recursive(src.path(), &dst_path).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst_path.join("file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("subdir").join("nested.txt")).unwrap(),
            "world"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_features: no features in config
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn resolve_features_empty_config() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));
        let config = json!({"image": "ubuntu:22.04"});

        let result = resolve_features(
            &config,
            Path::new("/workspace/devcontainer.json"),
            &Platform {
                os: "linux".to_string(),
                architecture: "amd64".to_string(),
            },
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await
        .unwrap();

        assert!(result.features.is_empty());
        assert!(result.dockerfile.is_empty());
    }

    #[tokio::test]
    async fn resolve_features_empty_features_object() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));
        let config = json!({"image": "ubuntu:22.04", "features": {}});

        let result = resolve_features(
            &config,
            Path::new("/workspace/devcontainer.json"),
            &Platform {
                os: "linux".to_string(),
                architecture: "amd64".to_string(),
            },
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await
        .unwrap();

        assert!(result.features.is_empty());
    }

    // -----------------------------------------------------------------------
    // resolve_features: local features end-to-end
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn resolve_features_local_feature() {
        let feature_dir = tempfile::tempdir().unwrap();
        let feature_path = feature_dir.path().join("my-feature");
        std::fs::create_dir_all(&feature_path).unwrap();
        std::fs::write(
            feature_path.join("devcontainer-feature.json"),
            r#"{"id": "my-feature", "version": "1.0.0", "options": {"version": {"type": "string", "default": "latest"}}}"#,
        )
        .unwrap();
        std::fs::write(feature_path.join("install.sh"), "#!/bin/sh\necho installed").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));

        // Config references the local feature with a relative path.
        let config_path = feature_dir.path().join("devcontainer.json");
        let local_ref = "./my-feature".to_string();
        let config = json!({
            "image": "ubuntu:22.04",
            "features": {
                local_ref: {"version": "18"}
            }
        });

        let platform = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
        };

        let result = resolve_features(
            &config,
            &config_path,
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(result.features.len(), 1);
        assert_eq!(result.features[0].id, "my-feature");
        assert!(result.features[0].has_install_script);
        assert_eq!(
            result.features[0].user_options.get("version"),
            Some(&json!("18"))
        );

        // Verify build context was created.
        assert!(result.build_context.exists());
        assert!(result.build_context.join("Dockerfile.features").exists());
        assert!(result.build_context.join("my-feature").exists());
        assert!(
            result
                .build_context
                .join("my-feature")
                .join("install.sh")
                .exists()
        );

        // Verify Dockerfile contains the feature.
        assert!(result.dockerfile.contains("my-feature"));

        // Verify new build context files: builtin env, per-feature env, wrapper script.
        assert!(
            result
                .build_context
                .join("devcontainer-features.builtin.env")
                .exists()
        );
        assert!(
            result
                .build_context
                .join("my-feature")
                .join("devcontainer-features.env")
                .exists()
        );
        assert!(
            result
                .build_context
                .join("my-feature")
                .join("devcontainer-features-install.sh")
                .exists()
        );

        // Verify builtin env content.
        let builtin_env = std::fs::read_to_string(
            result
                .build_context
                .join("devcontainer-features.builtin.env"),
        )
        .unwrap();
        assert!(builtin_env.contains("_CONTAINER_USER=root"));
        assert!(builtin_env.contains("_REMOTE_USER=root"));

        // Verify wrapper script sources env files.
        let wrapper = std::fs::read_to_string(
            result
                .build_context
                .join("my-feature")
                .join("devcontainer-features-install.sh"),
        )
        .unwrap();
        assert!(wrapper.contains(". ../devcontainer-features.builtin.env"));
        assert!(wrapper.contains(". ./devcontainer-features.env"));
        assert!(wrapper.contains("./install.sh"));

        // Verify metadata label is valid JSON.
        let label_parsed: serde_json::Value = serde_json::from_str(&result.metadata_label).unwrap();
        assert!(label_parsed.is_array());
    }

    #[tokio::test]
    async fn resolve_features_metadata_only_local() {
        let feature_dir = tempfile::tempdir().unwrap();
        let feature_path = feature_dir.path().join("meta-only");
        std::fs::create_dir_all(&feature_path).unwrap();
        std::fs::write(
            feature_path.join("devcontainer-feature.json"),
            r#"{"id": "meta-only", "version": "1.0.0", "containerEnv": {"FOO": "bar"}}"#,
        )
        .unwrap();
        // No install.sh -- metadata-only feature.

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));

        let config_path = feature_dir.path().join("devcontainer.json");
        let config = json!({
            "image": "ubuntu:22.04",
            "features": {
                "./meta-only": {}
            }
        });

        let platform = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
        };

        let result = resolve_features(
            &config,
            &config_path,
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(result.features.len(), 1);
        assert!(!result.features[0].has_install_script);
        assert_eq!(
            result.container_config.container_env.get("FOO").unwrap(),
            "bar"
        );
    }

    #[tokio::test]
    async fn resolve_features_multiple_local_ordering() {
        let feature_dir = tempfile::tempdir().unwrap();

        // Feature A: installs after B.
        let a_path = feature_dir.path().join("feat-a");
        std::fs::create_dir_all(&a_path).unwrap();
        std::fs::write(
            a_path.join("devcontainer-feature.json"),
            r#"{"id": "feat-a", "version": "1.0.0", "installsAfter": ["feat-b"]}"#,
        )
        .unwrap();
        std::fs::write(a_path.join("install.sh"), "#!/bin/sh").unwrap();

        // Feature B: no deps.
        let b_path = feature_dir.path().join("feat-b");
        std::fs::create_dir_all(&b_path).unwrap();
        std::fs::write(
            b_path.join("devcontainer-feature.json"),
            r#"{"id": "feat-b", "version": "1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(b_path.join("install.sh"), "#!/bin/sh").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));

        let config_path = feature_dir.path().join("devcontainer.json");
        let config = json!({
            "image": "ubuntu:22.04",
            "features": {
                "./feat-a": {},
                "./feat-b": {}
            }
        });

        let platform = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
        };

        let result = resolve_features(
            &config,
            &config_path,
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(result.features.len(), 2);

        let ids: Vec<&str> = result.features.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"feat-a"));
        assert!(ids.contains(&"feat-b"));
    }

    // -----------------------------------------------------------------------
    // resolve_features: invalid reference
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn resolve_features_invalid_ref() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));

        let config = json!({
            "image": "ubuntu:22.04",
            "features": {
                "not/valid": {}
            }
        });

        let platform = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
        };

        let result = resolve_features(
            &config,
            Path::new("/workspace/devcontainer.json"),
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            FeatureError::InvalidReference { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // End-to-end: OCI feature resolution (requires Docker + network)
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[cfg(feature = "integration-tests")]
    async fn e2e_resolve_node_feature() {
        let config = json!({
            "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
            "features": {
                "ghcr.io/devcontainers/features/node:1": {
                    "version": "20"
                }
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("devcontainer.json");
        std::fs::write(&config_path, config.to_string()).unwrap();

        let platform = test_platform();
        let cache = FeatureCache::new();

        let result = resolve_features(
            &config,
            &config_path,
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "mcr.microsoft.com/devcontainers/base:ubuntu",
                image_user: "root",
                metadata: None,
            },
            false,
        )
        .await;
        let resolved = result.expect("resolve_features should succeed");

        // Verify features
        assert_eq!(resolved.features.len(), 1);
        assert_eq!(resolved.features[0].id, "node");
        assert!(resolved.features[0].has_install_script);

        // Verify Dockerfile
        assert!(resolved.dockerfile.contains("dev_containers_target_stage"));
        assert!(resolved.dockerfile.contains("node"));

        // Verify build context exists
        assert!(resolved.build_context.exists());
        assert!(resolved.build_context.join("Dockerfile.features").exists());
        assert!(resolved.build_context.join("node").exists());

        // Verify metadata label is valid JSON array
        let label: serde_json::Value = serde_json::from_str(&resolved.metadata_label)
            .expect("metadata label should be valid JSON");
        assert!(label.is_array());
        assert!(!label.as_array().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // lifecycle_from_metadata_label
    // -----------------------------------------------------------------------

    #[test]
    fn lifecycle_label_empty_metadata() {
        let entries = lifecycle_from_metadata_label("", "postStartCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_label_invalid_json() {
        let entries = lifecycle_from_metadata_label("not json", "postStartCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_label_entry_with_feature_id() {
        let json = r#"[{"id": "node", "postStartCommand": "echo hello"}]"#;
        let entries = lifecycle_from_metadata_label(json, "postStartCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "node");
        assert_eq!(entries[0].command, json!("echo hello"));
    }

    #[test]
    fn lifecycle_label_entry_without_id() {
        let json = r#"[{"postStartCommand": "echo hello"}]"#;
        let entries = lifecycle_from_metadata_label(json, "postStartCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
    }

    #[test]
    fn lifecycle_label_skips_entries_without_phase() {
        let json = r#"[{"id": "node", "postCreateCommand": "echo create"}, {"id": "go"}]"#;
        let entries = lifecycle_from_metadata_label(json, "postStartCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_label_skips_null_phase() {
        let json = r#"[{"id": "node", "postStartCommand": null}]"#;
        let entries = lifecycle_from_metadata_label(json, "postStartCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_label_multiple_entries_mixed() {
        let json = r#"[
            {"id": "node", "postStartCommand": "echo node"},
            {"postStartCommand": "echo user"},
            {"id": "go"}
        ]"#;
        let entries = lifecycle_from_metadata_label(json, "postStartCommand");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].origin, "node");
        assert_eq!(entries[0].command, json!("echo node"));
        assert_eq!(entries[1].origin, "devcontainer.json");
        assert_eq!(entries[1].command, json!("echo user"));
    }
}
