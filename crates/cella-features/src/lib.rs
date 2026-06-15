pub mod auth;
pub mod cache;
pub mod dockerfile;
pub mod docs;
mod error;
pub mod fetch;
pub mod graph;
pub mod lockfile;
pub mod merge;
pub mod metadata;
pub mod oci;
pub mod ordering;
pub mod package;
pub mod publish;
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
pub use lockfile::{
    Lockfile, LockfileEntry, LockfileError, LockfilePolicy, compare_lockfile, generate_lockfile,
    lockfile_path, read_lockfile, write_lockfile,
};
pub use merge::{
    ImageMetadataUserInfo, merge_features, merge_with_devcontainer, parse_image_metadata,
    validate_options,
};
pub use metadata::parse_feature_metadata;
pub use oci::{FeatureFetcher, OciFetcher};
pub use ordering::compute_install_order;
pub use reference::{FeatureRef, NormalizedRef, feature_id_without_version};
pub use types::*;

use std::collections::{HashMap, HashSet};
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

/// Controls which keys are omitted from the generated `devcontainer.metadata`
/// image label.
///
/// Each field corresponds to one CLI flag; `Default` yields full output
/// (nothing omitted). New omit axes are added as fields — a struct-literal
/// change that the in-workspace constructors are updated for together (this is a
/// `pub` struct, so a new field is not source-compatible for outside callers,
/// but cella keeps all callers in-tree).
#[derive(Clone, Copy, Default)]
pub struct MetadataOmit {
    /// `--omit-config-remote-env-from-metadata`: strip `remoteEnv` from the
    /// user-config entry so host-specific or secret values are not persisted.
    /// Wired on `up` only; `build` leaves this false.
    pub remote_env: bool,
    /// `--skip-persisting-customizations-from-features`: strip `customizations`
    /// from per-feature entries only (not the user-config entry).
    /// Wired on `build` only; `up` leaves this false.
    pub feature_customizations: bool,
}

/// Context about the base image for feature resolution.
pub struct BaseImageContext<'a> {
    /// The base image reference (e.g., stage name or `ubuntu:22.04`).
    pub base_image: &'a str,
    /// The user in the base image (e.g., `"root"`, `"vscode"`).
    pub image_user: &'a str,
    /// `devcontainer.metadata` label from the base image, if available.
    pub metadata: Option<&'a str>,
    /// Which keys to omit from the generated `devcontainer.metadata` label.
    pub omit: MetadataOmit,
}

/// OCI digest metadata captured for a feature so it can be pinned in the
/// lockfile. Present only for OCI-sourced features (HTTP / local have no
/// digest to lock).
struct OciLockInfo {
    /// The OCI tag that was fetched (e.g. `"1"`, `"latest"`).
    version: String,
    /// The OCI registry hostname.
    registry: String,
    /// The OCI repository path.
    repository: String,
    /// The resolved manifest digest, e.g. `"sha256:abc..."`.
    digest: String,
}

/// Intermediate representation of a parsed feature before ordering.
struct FeatureEntry {
    /// Unique install identity = `(normalized_ref, options)`.
    ///
    /// The same feature pulled in twice with **different** options is two
    /// distinct installs per the spec, so a raw ref alone is not a unique key:
    /// it would collapse the two variants in any map keyed by it (ordering,
    /// assembly). `install_id` keeps them apart — see [`install_identity`].
    install_id: String,
    metadata: FeatureMetadata,
    artifact_dir: PathBuf,
    user_options: HashMap<String, serde_json::Value>,
    /// The raw reference the user (or a `dependsOn` key) wrote, kept for
    /// display and surfaced on the [`ResolvedFeature`].
    original_ref: String,
    /// OCI pin metadata, present only when this entry was fetched from an OCI
    /// registry. Drives lockfile generation, including transitive `dependsOn`
    /// features (which are appended to the install set during expansion).
    oci: Option<OciLockInfo>,
    /// The full OCI image manifest, present only for OCI-fetched features.
    /// Retained for `read-configuration --include-features-configuration`.
    oci_manifest: Option<oci_distribution::manifest::OciImageManifest>,
}

/// Build the unique install identity for a feature instance.
///
/// Two installs are the same feature iff their **normalized reference** and
/// their **options** match (devcontainer spec identity). The two parts are
/// joined with a NUL byte, which cannot appear in a ref or in serialized JSON
/// text, so the composite is unambiguous and stable.
fn install_identity(normalized_ref: &str, options_json: &str) -> String {
    format!("{normalized_ref}\u{0}{options_json}")
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
    lockfile_policy: LockfilePolicy,
) -> Result<ResolvedFeatures, FeatureError> {
    // Step 1: Extract the "features" object from the config.
    let features_obj = match config.get("features").and_then(|v| v.as_object()) {
        Some(obj) if !obj.is_empty() => obj,
        _ => {
            return resolve_empty_features(
                config,
                cache,
                base_image_ctx.metadata,
                base_image_ctx.omit,
            );
        }
    };

    let workspace_root = config_path.parent().unwrap_or_else(|| Path::new("."));

    // Step 2: Parse each key with FeatureRef::parse and normalize.
    let parsed_entries = parse_and_normalize(features_obj, workspace_root)?;

    // Step 3: Compute config hash for the build context path.
    let config_hash = compute_config_hash(features_obj);
    let build_context = cache.build_context_path(&config_hash);

    // Step 4: Read the existing lockfile (no-op for NoLockfile/Upgrade). Its
    // pinned digests, keyed by verbatim feature ref, drive digest pinning for
    // both top-level and transitive `dependsOn` features.
    let existing_lockfile = read_existing_lockfile(config_path, lockfile_policy)?;

    // Step 5: Fetch all top-level features concurrently (OCI pins to the locked
    // digest when one exists for that ref).
    let fetch_results =
        fetch_all_with_results(&parsed_entries, platform, cache, existing_lockfile.as_ref())
            .await?;

    // Step 6: Parse metadata, validate options, and capture OCI pin metadata.
    let mut feature_entries = parse_metadata_and_validate(&parsed_entries, &fetch_results)?;

    // Step 6b: Expand the install set with transitive `dependsOn` features,
    // pinning them to the lockfile too.
    expand_depends_on(
        &mut feature_entries,
        workspace_root,
        platform,
        cache,
        existing_lockfile.as_ref(),
    )
    .await?;

    // Step 7: Compute install order.
    let ordered_ids = compute_order(&feature_entries, config, workspace_root)?;

    // Step 8: Assemble resolved features in install order.
    let resolved = assemble_resolved(&feature_entries, &ordered_ids);

    // Step 9: Build and apply lockfile policy. The lockfile is generated from
    // the full install set (top-level + expanded `dependsOn`), so transitive
    // OCI dependencies are pinned and surfaced via each entry's `dependsOn`.
    let generated_lockfile = build_lockfile_from_entries(&feature_entries);
    let final_lockfile = apply_lockfile_policy(
        lockfile_policy,
        config_path,
        existing_lockfile,
        generated_lockfile,
    )?;

    // Step 10: Generate Dockerfile and build context.
    let dockerfile = generate_and_write_build_context(
        &build_context,
        &resolved,
        config,
        base_image_ctx.base_image,
        base_image_ctx.image_user,
        base_image_ctx.metadata,
        use_named_content_source,
    )?;

    // Step 11: Merge feature metadata and generate label.
    let container_config = merge_all_metadata(&resolved, config, base_image_ctx.metadata);
    let metadata_label = generate_metadata_label(
        &resolved,
        config,
        base_image_ctx.metadata,
        base_image_ctx.omit,
    );

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
        lockfile: final_lockfile,
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
///
/// `omit` controls selective key removal:
/// - `omit.remote_env` (`--omit-config-remote-env-from-metadata`): strips
///   `remoteEnv` from the user-config entry so host-specific or secret values
///   are not persisted. Mirrors the official CLI's `pickConfigProperties`
///   whitelist exclusion. Affects ONLY the user-config entry, never the
///   runtime `dev.cella.remote_env` label.
/// - `omit.feature_customizations` (`--skip-persisting-customizations-from-features`):
///   strips `customizations` from per-feature entries only (not the
///   user-config entry). Mirrors the official CLI's
///   `skipFeaturesCustomizationsKindMetadata` flag.
pub fn generate_metadata_label(
    features: &[ResolvedFeature],
    user_config: &serde_json::Value,
    base_image_metadata: Option<&str>,
    omit: MetadataOmit,
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
        entries.push(build_feature_metadata_entry(
            feature,
            omit.feature_customizations,
        ));
    }

    // Last element: user config properties. Optionally strip `remoteEnv`.
    let user_entry = if omit.remote_env {
        let mut stripped = user_config.clone();
        if let Some(obj) = stripped.as_object_mut() {
            obj.remove("remoteEnv");
        }
        stripped
    } else {
        user_config.clone()
    };
    entries.push(user_entry);

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
    omit: MetadataOmit,
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
        metadata_label: generate_metadata_label(&[], config, base_image_metadata, omit),
        lockfile: None,
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

/// Result of fetching a single feature — OCI fetches carry digest metadata,
/// other fetchers return only the artifact path.
enum FetchResult {
    Oci(Box<oci::OciFetchResult>),
    Other(PathBuf),
}

impl FetchResult {
    const fn artifact_dir(&self) -> &PathBuf {
        match self {
            Self::Oci(r) => &r.artifact_dir,
            Self::Other(p) => p,
        }
    }

    /// OCI pin metadata for this fetch, if it came from an OCI registry.
    fn oci_lock_info(&self) -> Option<OciLockInfo> {
        match self {
            Self::Oci(r) => Some(OciLockInfo {
                version: r.version.clone(),
                registry: r.registry.clone(),
                repository: r.repository.clone(),
                digest: r.digest.clone(),
            }),
            Self::Other(_) => None,
        }
    }

    /// The full OCI image manifest, if this fetch came from an OCI registry —
    /// retained for `read-configuration --include-features-configuration`
    /// (`featureSets[].sourceInformation.manifest`).
    fn oci_manifest(&self) -> Option<oci_distribution::manifest::OciImageManifest> {
        match self {
            Self::Oci(r) => Some(r.manifest.clone()),
            Self::Other(_) => None,
        }
    }
}

/// Look up the locked manifest digest for a verbatim feature ref.
///
/// The lockfile is keyed by the ref exactly as authored (tag included), so the
/// lookup is a direct map hit — a ref whose tag changed (`:1` → `:2`) simply
/// misses and is re-resolved against the registry, matching the official CLI.
fn locked_digest_for<'a>(existing: Option<&'a Lockfile>, raw_ref: &str) -> Option<&'a str> {
    existing?
        .features
        .get(raw_ref)
        .map(|e| e.integrity.as_str())
}

async fn fetch_all_with_results(
    parsed_entries: &[(String, NormalizedRef, serde_json::Value)],
    platform: &Platform,
    cache: &FeatureCache,
    existing_lockfile: Option<&Lockfile>,
) -> Result<Vec<FetchResult>, FeatureError> {
    let oci_fetcher = OciFetcher::default();
    let http_fetcher = HttpFetcher;
    let local_fetcher = LocalFetcher;

    let fetch_futures: Vec<_> = parsed_entries
        .iter()
        .map(|(key, norm_ref, _)| {
            let locked = locked_digest_for(existing_lockfile, key);
            fetch_one_with_digest(
                norm_ref,
                platform,
                cache,
                &oci_fetcher,
                &http_fetcher,
                locked,
                &local_fetcher,
            )
        })
        .collect();

    let results = join_all(fetch_futures).await;
    let mut fetch_results = Vec::with_capacity(parsed_entries.len());
    for result in results {
        fetch_results.push(result?);
    }
    Ok(fetch_results)
}

async fn fetch_one_with_digest(
    norm_ref: &NormalizedRef,
    platform: &Platform,
    cache: &FeatureCache,
    oci_fetcher: &OciFetcher,
    http_fetcher: &HttpFetcher,
    locked_digest: Option<&str>,
    local_fetcher: &LocalFetcher,
) -> Result<FetchResult, FeatureError> {
    debug!("fetching feature from {norm_ref}");
    match norm_ref {
        NormalizedRef::OciTarget { .. } => {
            let result = oci_fetcher
                .fetch_oci_with_digest(norm_ref, cache, locked_digest)
                .await?;
            Ok(FetchResult::Oci(Box::new(result)))
        }
        NormalizedRef::HttpTarget { .. } => {
            let path = http_fetcher.fetch(norm_ref, platform, cache).await?;
            Ok(FetchResult::Other(path))
        }
        NormalizedRef::LocalTarget { .. } => {
            let path = local_fetcher.fetch(norm_ref, platform, cache).await?;
            Ok(FetchResult::Other(path))
        }
    }
}

/// Read the existing lockfile from disk, honoring the policy.
///
/// Returns `Ok(None)` when no lockfile is consulted (`NoLockfile` / `Upgrade`)
/// or when the file is genuinely absent. A corrupt or unreadable lockfile is
/// surfaced as an error rather than treated as missing. Under
/// [`LockfilePolicy::Frozen`] an absent file is a hard [`LockfileError::Missing`].
fn read_existing_lockfile(
    config_path: &Path,
    policy: LockfilePolicy,
) -> Result<Option<Lockfile>, FeatureError> {
    if matches!(policy, LockfilePolicy::NoLockfile | LockfilePolicy::Upgrade) {
        return Ok(None);
    }

    let existing = read_lockfile(config_path).map_err(FeatureError::Lockfile)?;

    if matches!(policy, LockfilePolicy::Frozen) && existing.is_none() {
        return Err(FeatureError::Lockfile(LockfileError::Missing));
    }

    Ok(existing)
}

/// Build a lockfile from the full resolved install set.
///
/// Each OCI feature (top-level or pulled in transitively via `dependsOn`)
/// becomes one entry keyed by its verbatim ref. The `dependsOn` array lists the
/// raw refs of an entry's hard dependencies that resolved to OCI features, so
/// transitive pins are recorded and `--frozen-lockfile` validates them.
fn build_lockfile_from_entries(feature_entries: &[FeatureEntry]) -> Lockfile {
    // Refs that resolved to an OCI feature; a dependsOn edge is only recorded
    // when its target is itself lockable (OCI), matching the official CLI.
    let oci_refs: HashSet<&str> = feature_entries
        .iter()
        .filter(|e| e.oci.is_some())
        .map(|e| e.original_ref.as_str())
        .collect();

    let data: Vec<_> = feature_entries
        .iter()
        .filter_map(|entry| {
            let oci = entry.oci.as_ref()?;
            let resolved = format!("{}/{}@{}", oci.registry, oci.repository, oci.digest);
            let mut depends_on: Vec<String> = entry
                .metadata
                .depends_on
                .keys()
                .filter(|dep| oci_refs.contains(dep.as_str()))
                .cloned()
                .collect();
            depends_on.sort();
            depends_on.dedup();
            Some((
                entry.original_ref.clone(),
                // The lockfile records the resolved feature version (e.g.
                // `"1.7.1"`), not the OCI tag (`"1"`) — matching the official
                // `version: set.features[0].version`.
                entry.metadata.version.clone(),
                resolved,
                oci.digest.clone(),
                depends_on,
            ))
        })
        .collect();
    generate_lockfile(&data)
}

fn apply_lockfile_policy(
    policy: LockfilePolicy,
    config_path: &Path,
    existing: Option<Lockfile>,
    generated: Lockfile,
) -> Result<Option<Lockfile>, FeatureError> {
    if generated.features.is_empty() {
        return Ok(None);
    }
    match policy {
        LockfilePolicy::NoLockfile => Ok(None),
        LockfilePolicy::Update => {
            write_lockfile(config_path, &generated)?;
            Ok(Some(generated))
        }
        // `upgrade` regenerates the lockfile but the caller decides whether to
        // write it (so `upgrade --dry-run` can print without touching disk).
        LockfilePolicy::Upgrade => Ok(Some(generated)),
        LockfilePolicy::Frozen => {
            let existing = existing.ok_or(FeatureError::Lockfile(LockfileError::Missing))?;
            compare_lockfile(&existing, &generated).map_err(FeatureError::Lockfile)?;
            Ok(Some(existing))
        }
    }
}

/// Parse metadata from fetched artifacts and validate user options.
fn parse_metadata_and_validate(
    parsed_entries: &[(String, NormalizedRef, serde_json::Value)],
    fetch_results: &[FetchResult],
) -> Result<Vec<FeatureEntry>, FeatureError> {
    let mut entries = Vec::with_capacity(parsed_entries.len());

    for (i, (key, normalized, options_value)) in parsed_entries.iter().enumerate() {
        let fetch_result = &fetch_results[i];
        let artifact_dir = fetch_result.artifact_dir();
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

        // Identity keys on the NORMALIZED ref so shorthand and fully-qualified
        // spellings of the same feature deduplicate together, and on the raw
        // options value so option variants stay distinct.
        let options_json = serde_json::to_string(options_value).unwrap_or_default();
        entries.push(FeatureEntry {
            install_id: install_identity(&normalized.to_string(), &options_json),
            metadata,
            artifact_dir: artifact_dir.clone(),
            user_options,
            original_ref: key.clone(),
            oci: fetch_result.oci_lock_info(),
            oci_manifest: fetch_result.oci_manifest(),
        });
    }

    Ok(entries)
}

/// Compute install order from feature entries and config overrides.
///
/// The ordering graph is keyed by each entry's unique [`FeatureEntry::install_id`]
/// (not the raw ref), so two installs of the same feature with different
/// options stay distinct. Each entry's `dependsOn` / `installsAfter` edges are
/// resolved from raw refs to the matching install identities before sorting,
/// so an edge points at the specific option-variant instance it names.
///
/// # Errors
///
/// Returns [`FeatureError::CyclicDependsOn`] when a hard `dependsOn` cycle
/// is detected.
fn compute_order(
    feature_entries: &[FeatureEntry],
    config: &serde_json::Value,
    workspace_root: &Path,
) -> Result<Vec<String>, FeatureError> {
    let order_metadata = build_order_metadata(feature_entries, workspace_root);
    let order_input: Vec<(String, &FeatureMetadata)> = order_metadata
        .iter()
        .map(|(id, meta)| (id.clone(), meta))
        .collect();

    let override_order = resolve_override_order(config, feature_entries, workspace_root);

    let mut depends_on_cycle: Option<Vec<String>> = None;
    let (ordered_ids, order_warnings) = compute_install_order(
        &order_input,
        override_order.as_deref(),
        &mut depends_on_cycle,
    );

    if let Some(cycle) = depends_on_cycle {
        return Err(FeatureError::CyclicDependsOn { cycle });
    }

    for w in &order_warnings {
        warn!("install order: {w:?}");
    }

    Ok(ordered_ids)
}

/// Normalize a raw feature reference for identity matching, falling back to the
/// raw string when it cannot be parsed (invalid refs surface elsewhere).
fn normalize_ref_for_identity(raw: &str, workspace_root: &Path) -> String {
    FeatureRef::parse(raw)
        .and_then(|r| r.normalize(workspace_root))
        .map_or_else(|_| raw.to_owned(), |(n, _)| n.to_string())
}

/// Build per-entry ordering metadata keyed by `install_id`, with `dependsOn`
/// and `installsAfter` edges rewritten from raw refs to the matching
/// `install_id`s.
///
/// - `dependsOn` matches on `(normalized_ref, options)` — the full identity, so
///   it targets the exact option-variant instance it names.
/// - `installsAfter` matches on `normalized_ref` only (the spec gives it no
///   options); it therefore expands to **every** option-variant of that ref.
fn build_order_metadata(
    feature_entries: &[FeatureEntry],
    workspace_root: &Path,
) -> Vec<(String, FeatureMetadata)> {
    // Set of valid install identities, for dependsOn resolution.
    let identities: HashSet<&str> = feature_entries
        .iter()
        .map(|e| e.install_id.as_str())
        .collect();
    let by_ref = index_by_ref(feature_entries);

    feature_entries
        .iter()
        .map(|entry| {
            let mut meta = entry.metadata.clone();

            // Rewrite hard (dependsOn) edges to matching install identities.
            let depends_on = std::mem::take(&mut meta.depends_on);
            meta.depends_on = depends_on
                .into_iter()
                .filter_map(|(dep_ref, opts)| {
                    let norm = normalize_ref_for_identity(&dep_ref, workspace_root);
                    let opts_json = serde_json::to_string(&opts).unwrap_or_default();
                    let id = install_identity(&norm, &opts_json);
                    identities.contains(id.as_str()).then_some((id, opts))
                })
                .collect();

            // Rewrite soft (installsAfter) edges; one ref may fan out to
            // several option-variant installs.
            let installs_after = std::mem::take(&mut meta.installs_after);
            meta.installs_after = installs_after
                .into_iter()
                .flat_map(|after_ref| {
                    let norm = normalize_ref_for_identity(&after_ref, workspace_root);
                    by_ref
                        .get(norm.as_str())
                        .into_iter()
                        .flatten()
                        .map(|id| (*id).to_string())
                        .collect::<Vec<_>>()
                })
                .collect();

            (entry.install_id.clone(), meta)
        })
        .collect()
}

/// Index install identities by their normalized ref.
///
/// One ref may map to several identities when the same feature is installed
/// more than once with different options. Used to resolve option-agnostic
/// references (`installsAfter`, `overrideFeatureInstallOrder`) to the concrete
/// install identities they cover.
fn index_by_ref(feature_entries: &[FeatureEntry]) -> HashMap<&str, Vec<&str>> {
    let mut by_ref: HashMap<&str, Vec<&str>> = HashMap::new();
    for entry in feature_entries {
        // The install_id is `normalized_ref \0 options_json`; the ref is the
        // part before the NUL separator.
        let normalized_ref = entry
            .install_id
            .split('\u{0}')
            .next()
            .unwrap_or(&entry.install_id);
        by_ref
            .entry(normalized_ref)
            .or_default()
            .push(entry.install_id.as_str());
    }
    by_ref
}

/// Resolve `overrideFeatureInstallOrder` (raw refs) to install identities.
///
/// Each override ref names a feature without options, so it expands to every
/// matching option-variant install, preserving the override's declared order.
fn resolve_override_order(
    config: &serde_json::Value,
    feature_entries: &[FeatureEntry],
    workspace_root: &Path,
) -> Option<Vec<String>> {
    let raw = config
        .get("overrideFeatureInstallOrder")
        .and_then(|v| v.as_array())?;

    let by_ref = index_by_ref(feature_entries);

    let resolved: Vec<String> = raw
        .iter()
        .filter_map(serde_json::Value::as_str)
        .flat_map(|ref_str| {
            let norm = normalize_ref_for_identity(ref_str, workspace_root);
            by_ref
                .get(norm.as_str())
                .into_iter()
                .flatten()
                .map(|id| (*id).to_string())
                .collect::<Vec<_>>()
        })
        .collect();

    Some(resolved)
}

/// Assemble `ResolvedFeature` structs in install order.
///
/// `ordered_ids` are unique [`FeatureEntry::install_id`]s, so the lookup never
/// collapses two option-variants of the same ref onto one entry.
fn assemble_resolved(
    feature_entries: &[FeatureEntry],
    ordered_ids: &[String],
) -> Vec<ResolvedFeature> {
    let entry_map: HashMap<&str, usize> = feature_entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.install_id.as_str(), i))
        .collect();

    let mut resolved = Vec::with_capacity(ordered_ids.len());

    for id in ordered_ids {
        let Some(&idx) = entry_map.get(id.as_str()) else {
            // An ordered id with no backing entry would be a logic bug in the
            // ordering step; skip rather than panic.
            warn!("install order produced unknown id {id:?}; skipping");
            continue;
        };
        let entry = &feature_entries[idx];

        let has_install = entry.artifact_dir.join("install.sh").is_file();

        resolved.push(ResolvedFeature {
            id: entry.metadata.id.clone(),
            original_ref: entry.original_ref.clone(),
            metadata: entry.metadata.clone(),
            user_options: entry.user_options.clone(),
            artifact_dir: entry.artifact_dir.clone(),
            has_install_script: has_install,
            // An OCI feature carries both its resolved digest (from the lock
            // info) and the manifest blob; bundle them, or `None` for non-OCI.
            oci: match (&entry.oci, &entry.oci_manifest) {
                (Some(lock), Some(manifest)) => Some(ResolvedOciManifest {
                    registry: lock.registry.clone(),
                    repository: lock.repository.clone(),
                    version: lock.version.clone(),
                    digest: lock.digest.clone(),
                    manifest: manifest.clone(),
                }),
                _ => None,
            },
        });

        debug!(
            "resolved feature {} -> {} (install_script={})",
            entry.original_ref, entry.metadata.id, has_install
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
                generate_wrapper_script(feature),
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
// dependsOn expansion
// ---------------------------------------------------------------------------

/// Recursively expand `dependsOn` references into `feature_entries`.
///
/// Starting from the already-fetched user-declared features, this function
/// performs a worklist BFS over every `dependsOn` key found in each feature's
/// metadata.  For each referenced feature not yet in the install set it:
///
/// 1. Parses and normalises the reference (reusing the same path as
///    user-declared features).
/// 2. Fetches the artifact (OCI / HTTP / local) via the same fetchers.
/// 3. Parses its `devcontainer-feature.json` and validates options.
/// 4. Pushes the new entry onto `feature_entries` and enqueues its own
///    `dependsOn` keys for further expansion.
///
/// Identity / dedup semantics per spec: two references are the same feature
/// when their normalised reference string **and** their options JSON are
/// identical.  Same feature with different options = two separate install
/// entries (not merged).
async fn expand_depends_on(
    feature_entries: &mut Vec<FeatureEntry>,
    workspace_root: &Path,
    platform: &Platform,
    cache: &FeatureCache,
    existing_lockfile: Option<&Lockfile>,
) -> Result<(), FeatureError> {
    let oci_fetcher = OciFetcher::default();
    let http_fetcher = HttpFetcher;
    let local_fetcher = LocalFetcher;

    // Normalize a raw ref string for identity keying, ignoring parse errors
    // (invalid refs are caught later during fetch).
    let normalize_key = |raw: &str| normalize_ref_for_identity(raw, workspace_root);

    // Visited set: (normalised_ref_string, options_json) — spec identity
    // semantics.  Pre-populate with user-declared entries using their
    // NORMALISED ref so that spelling variants of the same OCI ref
    // (e.g. shorthand vs. fully-qualified) deduplicate correctly.
    let mut visited: HashSet<(String, String)> = feature_entries
        .iter()
        .map(|e| {
            let norm_key = normalize_key(&e.original_ref);
            let opts = serde_json::to_string(&e.user_options).unwrap_or_default();
            (norm_key, opts)
        })
        .collect();

    // Worklist: (raw_ref_string, options_value) pairs to process.
    // Pre-populate from the dependsOn of already-fetched entries.
    let mut worklist: Vec<(String, serde_json::Value)> = feature_entries
        .iter()
        .flat_map(|e| {
            e.metadata
                .depends_on
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
        })
        .collect();

    while let Some((dep_ref, dep_opts)) = worklist.pop() {
        // Parse and normalise the reference first so we can deduplicate on
        // the canonical identity, not the raw string the author wrote.
        let feature_ref = FeatureRef::parse(&dep_ref)?;
        let (normalized, warning) = feature_ref.normalize(workspace_root)?;
        if let Some(w) = warning {
            warn!("{dep_ref}: deprecated feature reference in dependsOn ({w:?})");
            let _ = w;
        }

        let opts_json = serde_json::to_string(&dep_opts).unwrap_or_default();
        let visit_key = (normalized.to_string(), opts_json.clone());

        if visited.contains(&visit_key) {
            continue;
        }
        visited.insert(visit_key);

        // Unique install identity for this expanded instance — same composite
        // as the `visited` key so option variants of one ref stay distinct.
        let install_id = install_identity(&normalized.to_string(), &opts_json);

        // Fetch the artifact, pinning to the locked digest (keyed by the
        // verbatim dependsOn ref) when the lockfile has one.
        let locked = locked_digest_for(existing_lockfile, &dep_ref);
        let fetch_result = fetch_one_with_digest(
            &normalized,
            platform,
            cache,
            &oci_fetcher,
            &http_fetcher,
            locked,
            &local_fetcher,
        )
        .await?;
        let artifact_dir = fetch_result.artifact_dir().clone();

        // Parse metadata.
        let metadata_path = artifact_dir.join("devcontainer-feature.json");
        let metadata_json =
            std::fs::read_to_string(&metadata_path).map_err(|e| FeatureError::InvalidMetadata {
                feature_id: dep_ref.clone(),
                reason: format!("cannot read devcontainer-feature.json: {e}"),
            })?;
        let metadata = parse_feature_metadata(&metadata_json)?;

        // Parse and validate user options.
        let user_options = parse_user_options(&dep_opts);
        let warnings = validate_options(&dep_ref, &user_options, &metadata.options);
        for w in &warnings {
            log_option_warning(&dep_ref, w);
        }

        // Enqueue this entry's own dependsOn for further expansion,
        // pre-filtering with the normalised key to avoid redundant fetches.
        for (transitive_ref, transitive_opts) in &metadata.depends_on {
            let t_norm = normalize_key(transitive_ref);
            let t_opts_json = serde_json::to_string(transitive_opts).unwrap_or_default();
            let t_key = (t_norm, t_opts_json);
            if !visited.contains(&t_key) {
                worklist.push((transitive_ref.clone(), transitive_opts.clone()));
            }
        }

        feature_entries.push(FeatureEntry {
            install_id,
            metadata,
            artifact_dir,
            user_options,
            original_ref: dep_ref,
            oci: fetch_result.oci_lock_info(),
            oci_manifest: fetch_result.oci_manifest(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Leaf helpers
// ---------------------------------------------------------------------------

/// Build a single metadata label entry for a resolved feature.
///
/// When `omit_customizations` is `true` (the
/// `--skip-persisting-customizations-from-features` flag), the `customizations`
/// key is excluded from the entry. All other fields are unaffected.
fn build_feature_metadata_entry(
    feature: &ResolvedFeature,
    omit_customizations: bool,
) -> serde_json::Value {
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

    if !omit_customizations && let Some(cust) = &feature.metadata.customizations {
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
    use std::collections::{HashMap, HashSet};

    use crate::LockfilePolicy;

    use serde_json::json;

    use super::*;

    use crate::test_utils::test_platform;

    // -----------------------------------------------------------------------
    // generate_metadata_label
    // -----------------------------------------------------------------------

    #[test]
    fn metadata_label_empty_features() {
        let label = generate_metadata_label(
            &[],
            &json!({"image": "ubuntu"}),
            None,
            MetadataOmit::default(),
        );
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
            oci: None,
        }];

        let label = generate_metadata_label(&features, &json!({}), None, MetadataOmit::default());
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "node");
        assert_eq!(parsed[0]["entrypoint"], "/init.sh");
        assert!(parsed[0]["containerEnv"]["NODE_PATH"].is_string());
    }

    #[test]
    fn metadata_label_with_base_image_metadata() {
        let base_meta = r#"[{"id":"base","containerEnv":{"LANG":"C.UTF-8"}}]"#;
        let label =
            generate_metadata_label(&[], &json!({}), Some(base_meta), MetadataOmit::default());
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();

        // base entry + user config entry
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "base");
    }

    #[test]
    fn metadata_label_base_image_non_array() {
        let base_meta = r#"{"id":"single"}"#;
        let label =
            generate_metadata_label(&[], &json!({}), Some(base_meta), MetadataOmit::default());
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "single");
    }

    #[test]
    fn metadata_label_omits_remote_env_when_requested() {
        let config = json!({
            "image": "ubuntu",
            "remoteEnv": {"SECRET": "value"},
            "remoteUser": "vscode",
        });

        // remote_env=true strips remoteEnv from the user-config entry...
        let omitted = generate_metadata_label(
            &[],
            &config,
            None,
            MetadataOmit {
                remote_env: true,
                ..Default::default()
            },
        );
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&omitted).unwrap();
        let entry = parsed.last().unwrap();
        assert!(entry.get("remoteEnv").is_none());
        // ...without disturbing the other keys.
        assert_eq!(entry["remoteUser"], "vscode");
        assert_eq!(entry["image"], "ubuntu");

        // default (omit nothing) retains remoteEnv.
        let kept = generate_metadata_label(&[], &config, None, MetadataOmit::default());
        let parsed_kept: Vec<serde_json::Value> = serde_json::from_str(&kept).unwrap();
        assert_eq!(parsed_kept.last().unwrap()["remoteEnv"]["SECRET"], "value");
    }

    #[test]
    fn metadata_label_omit_remote_env_is_noop_without_key() {
        // A config that has no remoteEnv must not panic and must be unchanged.
        let config = json!({"image": "ubuntu"});
        let label = generate_metadata_label(
            &[],
            &config,
            None,
            MetadataOmit {
                remote_env: true,
                ..Default::default()
            },
        );
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();
        assert_eq!(parsed.last().unwrap()["image"], "ubuntu");
        assert!(parsed.last().unwrap().get("remoteEnv").is_none());
    }

    #[test]
    fn metadata_label_omits_feature_customizations_when_requested() {
        let features = vec![ResolvedFeature {
            id: "python".to_string(),
            original_ref: "ghcr.io/devcontainers/features/python:1".to_string(),
            metadata: FeatureMetadata {
                id: "python".to_string(),
                customizations: Some(json!({"vscode": {"extensions": ["ms-python.python"]}})),
                entrypoint: Some("/init.sh".to_string()),
                ..Default::default()
            },
            user_options: HashMap::new(),
            artifact_dir: PathBuf::from("/tmp/features/python"),
            has_install_script: true,
            oci: None,
        }];
        let user_config = json!({
            "image": "ubuntu",
            "customizations": {"vscode": {"extensions": ["user.ext"]}},
        });

        // flag on: feature entry drops customizations; user-config entry keeps them.
        let omit = MetadataOmit {
            feature_customizations: true,
            ..Default::default()
        };
        let label = generate_metadata_label(&features, &user_config, None, omit);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&label).unwrap();
        // [feature_entry, user_config_entry]
        assert_eq!(parsed.len(), 2);
        let feature_entry = &parsed[0];
        let user_entry = &parsed[1];
        assert!(
            feature_entry.get("customizations").is_none(),
            "feature customizations must be absent when flag is set"
        );
        assert_eq!(feature_entry["id"], "python");
        assert_eq!(feature_entry["entrypoint"], "/init.sh");
        // user-config customizations must be untouched.
        assert!(
            user_entry.get("customizations").is_some(),
            "user-config customizations must be retained"
        );

        // flag off: both present.
        let label_full =
            generate_metadata_label(&features, &user_config, None, MetadataOmit::default());
        let parsed_full: Vec<serde_json::Value> = serde_json::from_str(&label_full).unwrap();
        assert!(
            parsed_full[0].get("customizations").is_some(),
            "feature customizations must be present when flag is not set"
        );
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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

    #[cella_testing::runtime_test(network)]
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
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

    #[tokio::test]
    async fn resolve_features_preserves_devcontainer_id_variable_in_mounts() {
        let feature_dir = tempfile::tempdir().unwrap();
        let feature_path = feature_dir.path().join("dind-test");
        std::fs::create_dir_all(&feature_path).unwrap();
        std::fs::write(
            feature_path.join("devcontainer-feature.json"),
            r#"{
                "id": "dind-test",
                "version": "1.0.0",
                "mounts": [
                    {
                        "source": "dind-var-lib-docker-${devcontainerId}",
                        "target": "/var/lib/docker",
                        "type": "volume"
                    }
                ]
            }"#,
        )
        .unwrap();
        std::fs::write(feature_path.join("install.sh"), "#!/bin/sh\necho ok").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));
        let config_path = feature_dir.path().join("devcontainer.json");
        let config = json!({
            "image": "ubuntu:22.04",
            "features": { "./dind-test": {} }
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
        )
        .await
        .unwrap();

        assert!(
            result
                .container_config
                .mounts
                .iter()
                .any(|m| m.contains("${devcontainerId}")),
            "mounts should contain raw ${{devcontainerId}} before orchestrator substitution: {:?}",
            result.container_config.mounts
        );
    }

    // -----------------------------------------------------------------------
    // resolve_features: dependsOn expansion
    // -----------------------------------------------------------------------

    /// Build a minimal local feature dir with optional dependsOn and install.sh.
    fn make_local_feature(
        base: &Path,
        name: &str,
        depends_on: &[(&str, serde_json::Value)],
        has_install: bool,
    ) {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let mut meta = serde_json::json!({
            "id": name,
            "version": "1.0.0"
        });
        if !depends_on.is_empty() {
            let deps: serde_json::Map<String, serde_json::Value> = depends_on
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect();
            meta.as_object_mut()
                .unwrap()
                .insert("dependsOn".to_string(), serde_json::Value::Object(deps));
        }
        std::fs::write(path.join("devcontainer-feature.json"), meta.to_string()).unwrap();
        if has_install {
            std::fs::write(path.join("install.sh"), "#!/bin/sh\necho ok").unwrap();
        }
    }

    #[tokio::test]
    async fn depends_on_transitive_dep_auto_added_and_ordered_first() {
        // Feature "child" depends on "parent". Only "child" is declared in
        // devcontainer.json. After expansion both should be resolved, and
        // "parent" must appear before "child".
        let feature_dir = tempfile::tempdir().unwrap();

        make_local_feature(feature_dir.path(), "parent", &[], true);
        // child declares dependsOn on ./parent
        let child_dep_ref = "./parent".to_string();
        make_local_feature(
            feature_dir.path(),
            "child",
            &[(&child_dep_ref, serde_json::json!({}))],
            true,
        );

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));
        let config_path = feature_dir.path().join("devcontainer.json");
        let config = json!({
            "image": "ubuntu:22.04",
            "features": { "./child": {} }
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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
        )
        .await
        .unwrap();

        let ids: Vec<&str> = result.features.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "both child and parent should be resolved");
        assert!(ids.contains(&"parent"));
        assert!(ids.contains(&"child"));

        let parent_pos = ids.iter().position(|&x| x == "parent").unwrap();
        let child_pos = ids.iter().position(|&x| x == "child").unwrap();
        assert!(
            parent_pos < child_pos,
            "parent must be installed before child, got order: {ids:?}"
        );
    }

    #[tokio::test]
    async fn depends_on_dedup_same_ref_same_options() {
        // Two features both declare dependsOn on the same "shared" feature with
        // the same options. It should only appear once in the install set.
        let feature_dir = tempfile::tempdir().unwrap();

        make_local_feature(feature_dir.path(), "shared", &[], true);

        let shared_ref = "./shared".to_string();
        make_local_feature(
            feature_dir.path(),
            "feat-a",
            &[(&shared_ref, serde_json::json!({}))],
            true,
        );
        make_local_feature(
            feature_dir.path(),
            "feat-b",
            &[(&shared_ref, serde_json::json!({}))],
            true,
        );

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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
        )
        .await
        .unwrap();

        let ids: Vec<&str> = result.features.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids.iter().filter(|&&x| x == "shared").count(),
            1,
            "shared should appear exactly once, got: {ids:?}"
        );
        assert_eq!(ids.len(), 3, "feat-a, feat-b, shared: {ids:?}");
    }

    #[tokio::test]
    async fn depends_on_same_ref_different_options_two_entries() {
        // Same feature referenced with different options = two separate entries.
        let feature_dir = tempfile::tempdir().unwrap();

        make_local_feature(feature_dir.path(), "base-tool", &[], true);

        // feat-a depends on base-tool with version="1"
        let base_ref = "./base-tool".to_string();
        make_local_feature(
            feature_dir.path(),
            "feat-a",
            &[(&base_ref, serde_json::json!({"version": "1"}))],
            true,
        );
        // feat-b depends on base-tool with version="2"
        make_local_feature(
            feature_dir.path(),
            "feat-b",
            &[(&base_ref, serde_json::json!({"version": "2"}))],
            true,
        );

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
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
        )
        .await
        .unwrap();

        let ids: Vec<&str> = result.features.iter().map(|f| f.id.as_str()).collect();
        // Per spec: same feature with different options = different entries.
        assert_eq!(
            ids.iter().filter(|&&x| x == "base-tool").count(),
            2,
            "base-tool with different options should appear twice, got: {ids:?}"
        );
        assert_eq!(ids.len(), 4);

        // Regression (unique-key fix): the two base-tool installs must keep
        // their DISTINCT options. With a non-unique key both collapsed onto one
        // entry and the surviving copy carried the wrong option set.
        let base_versions: HashSet<Option<&str>> = result
            .features
            .iter()
            .filter(|f| f.id == "base-tool")
            .map(|f| f.user_options.get("version").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            base_versions,
            HashSet::from([Some("1"), Some("2")]),
            "both base-tool option variants must survive distinctly, got: {base_versions:?}"
        );
    }

    #[tokio::test]
    async fn depends_on_cycle_returns_error() {
        // Feature A depends on B, B depends on A → fatal error.
        let feature_dir = tempfile::tempdir().unwrap();

        let a_ref = "./feat-a".to_string();
        let b_ref = "./feat-b".to_string();
        make_local_feature(
            feature_dir.path(),
            "feat-a",
            &[(&b_ref, serde_json::json!({}))],
            true,
        );
        make_local_feature(
            feature_dir.path(),
            "feat-b",
            &[(&a_ref, serde_json::json!({}))],
            true,
        );

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

        let err = resolve_features(
            &config,
            &config_path,
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, FeatureError::CyclicDependsOn { .. }),
            "expected CyclicDependsOn error, got: {err:?}"
        );
    }

    /// Regression: transitive cycle discovered during dependsOn expansion where
    /// only the root feature is user-declared.
    ///
    /// User declares only A.  A dependsOn B (not declared).  B dependsOn A
    /// (back-edge, completing the cycle).  `resolve_features` must return
    /// `CyclicDependsOn`, not loop or silently mis-order.
    #[tokio::test]
    async fn depends_on_transitive_cycle_not_user_declared_returns_error() {
        let feature_dir = tempfile::tempdir().unwrap();

        let a_ref = "./feat-a".to_string();
        let b_ref = "./feat-b".to_string();

        // A depends on B (B is not in the user's features list).
        make_local_feature(
            feature_dir.path(),
            "feat-a",
            &[(&b_ref, serde_json::json!({}))],
            true,
        );
        // B depends back on A — completes the cycle.
        make_local_feature(
            feature_dir.path(),
            "feat-b",
            &[(&a_ref, serde_json::json!({}))],
            true,
        );

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path().join("features"));
        let config_path = feature_dir.path().join("devcontainer.json");
        // Only feat-a is declared — B is auto-injected via dependsOn expansion.
        let config = json!({
            "image": "ubuntu:22.04",
            "features": { "./feat-a": {} }
        });
        let platform = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
        };

        let err = resolve_features(
            &config,
            &config_path,
            &platform,
            &cache,
            &BaseImageContext {
                base_image: "ubuntu:22.04",
                image_user: "root",
                metadata: None,
                omit: MetadataOmit::default(),
            },
            false,
            LockfilePolicy::NoLockfile,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, FeatureError::CyclicDependsOn { .. }),
            "expected CyclicDependsOn error for transitive cycle, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // lockfile generation from the resolved install set
    // -----------------------------------------------------------------------

    /// Build a minimal OCI-backed [`FeatureEntry`] for lockfile tests.
    fn oci_entry(
        original_ref: &str,
        registry: &str,
        repo: &str,
        tag: &str,
        digest: &str,
    ) -> FeatureEntry {
        FeatureEntry {
            install_id: install_identity(original_ref, "{}"),
            metadata: FeatureMetadata {
                id: repo.rsplit('/').next().unwrap_or(repo).to_string(),
                // The resolved feature version (from devcontainer-feature.json),
                // distinct from the OCI tag — this is what the lockfile records.
                version: "1.7.1".to_string(),
                ..Default::default()
            },
            artifact_dir: PathBuf::from("/tmp/feature"),
            user_options: HashMap::new(),
            original_ref: original_ref.to_string(),
            oci: Some(OciLockInfo {
                version: tag.to_string(),
                registry: registry.to_string(),
                repository: repo.to_string(),
                digest: digest.to_string(),
            }),
            oci_manifest: None,
        }
    }

    #[test]
    fn lockfile_keys_on_verbatim_ref_including_tag() {
        // Per the devcontainer spec the lockfile key is the ref exactly as
        // authored — the `:1` tag is part of the key, never stripped.
        let entries = vec![oci_entry(
            "ghcr.io/devcontainers/features/node:1",
            "ghcr.io",
            "devcontainers/features/node",
            "1",
            "sha256:abc",
        )];
        let lf = build_lockfile_from_entries(&entries);
        assert!(
            lf.features
                .contains_key("ghcr.io/devcontainers/features/node:1")
        );
        let entry = &lf.features["ghcr.io/devcontainers/features/node:1"];
        // The key keeps the authored tag (`:1`), but `version` is the resolved
        // feature version (matching the official `set.features[0].version`).
        assert_eq!(entry.version, "1.7.1");
        assert_eq!(entry.integrity, "sha256:abc");
        assert_eq!(
            entry.resolved,
            "ghcr.io/devcontainers/features/node@sha256:abc"
        );
    }

    #[test]
    fn lockfile_records_transitive_depends_on() {
        // A feature whose `dependsOn` names another OCI feature must record
        // that dependency both as its own lockfile entry and in the parent's
        // `dependsOn` array — keyed by the verbatim dep ref.
        let mut parent = oci_entry("ghcr.io/x/parent:1", "ghcr.io", "x/parent", "1", "sha256:p");
        parent
            .metadata
            .depends_on
            .insert("ghcr.io/x/child".to_string(), json!({}));
        let child = oci_entry(
            "ghcr.io/x/child",
            "ghcr.io",
            "x/child",
            "latest",
            "sha256:c",
        );

        let lf = build_lockfile_from_entries(&[parent, child]);

        assert_eq!(
            lf.features.len(),
            2,
            "both parent and transitive dep recorded"
        );
        assert!(lf.features.contains_key("ghcr.io/x/child"));
        assert_eq!(
            lf.features["ghcr.io/x/parent:1"].depends_on,
            vec!["ghcr.io/x/child".to_string()]
        );
        // The transitive dep has no further deps.
        assert!(lf.features["ghcr.io/x/child"].depends_on.is_empty());
    }

    #[test]
    fn lockfile_depends_on_excludes_non_oci_targets() {
        // A dependsOn edge to a non-OCI (local) feature is not lockable, so it
        // must not appear in the parent's dependsOn array.
        let mut parent = oci_entry("ghcr.io/x/parent:1", "ghcr.io", "x/parent", "1", "sha256:p");
        parent
            .metadata
            .depends_on
            .insert("./local-feature".to_string(), json!({}));
        let lf = build_lockfile_from_entries(&[parent]);
        assert!(lf.features["ghcr.io/x/parent:1"].depends_on.is_empty());
    }

    #[test]
    fn lockfile_skips_non_oci_features() {
        // HTTP / local features carry no digest and produce no lockfile entry.
        let entry = FeatureEntry {
            install_id: install_identity("./local", "{}"),
            metadata: FeatureMetadata {
                id: "local".to_string(),
                ..Default::default()
            },
            artifact_dir: PathBuf::from("/tmp/local"),
            user_options: HashMap::new(),
            original_ref: "./local".to_string(),
            oci: None,
            oci_manifest: None,
        };
        let lf = build_lockfile_from_entries(&[entry]);
        assert!(lf.features.is_empty());
    }

    // -----------------------------------------------------------------------
    // locked_digest_for
    // -----------------------------------------------------------------------

    #[test]
    fn locked_digest_lookup_uses_verbatim_ref() {
        let lf = generate_lockfile(&[(
            "ghcr.io/x/y:1".to_string(),
            "1".to_string(),
            "ghcr.io/x/y@sha256:aa".to_string(),
            "sha256:aa".to_string(),
            vec![],
        )]);
        // Exact tagged ref hits.
        assert_eq!(
            locked_digest_for(Some(&lf), "ghcr.io/x/y:1"),
            Some("sha256:aa")
        );
        // A changed tag (`:1` -> `:2`) misses, so resolution re-fetches `:2`.
        assert_eq!(locked_digest_for(Some(&lf), "ghcr.io/x/y:2"), None);
        // Untagged form misses too — the key is verbatim.
        assert_eq!(locked_digest_for(Some(&lf), "ghcr.io/x/y"), None);
        // No lockfile -> never pinned.
        assert_eq!(locked_digest_for(None, "ghcr.io/x/y:1"), None);
    }
}
