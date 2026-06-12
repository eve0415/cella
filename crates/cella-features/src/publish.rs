//! `features publish` — package and push devcontainer features to an OCI registry.
//!
//! Implements the same contract as `devcontainer features publish`:
//! - Package the feature source tree with `package::package`.
//! - For each feature: compute the semver tag fan-out against already-published
//!   tags, skip if the exact version already exists, push the tgz layer + config
//!   + manifest, then publish the collection index.
//! - Emit a JSON summary to stdout: `{ "<id>": { publishedTags, digest, version } }`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use semver::Version;
use serde_json::{Value, json};
use tracing::{debug, warn};

use cella_oci::{LayerSpec, PushError, list_published_tags, push_artifact};

use crate::package::{PackageError, PackageOptions, PackagedFeature, package};

// ---------------------------------------------------------------------------
// Media types (per devcontainer spec)
// ---------------------------------------------------------------------------

const MEDIA_TYPE_LAYER: &str = "application/vnd.devcontainers.layer.v1+tar";
const MEDIA_TYPE_COLLECTION_LAYER: &str = "application/vnd.devcontainers.collection.layer.v1+json";
const MEDIA_TYPE_CONFIG: &str = "application/vnd.devcontainers";
const GHCR_IO: &str = "ghcr.io";

// ---------------------------------------------------------------------------
// Public API types
// ---------------------------------------------------------------------------

/// Options for `features publish`.
#[derive(Debug, Clone)]
pub struct PublishOptions {
    /// Path to single feature directory or collection root (default: `.`).
    pub target: PathBuf,
    /// OCI registry host (default: `ghcr.io`).
    pub registry: String,
    /// Namespace within the registry, e.g. `owner/repo`.
    pub namespace: String,
}

/// Per-feature result in the JSON output.
#[derive(Debug, Clone)]
pub struct FeaturePublishResult {
    /// Manifest digest (`sha256:…`), empty if skipped.
    pub digest: String,
    /// Tags that were actually pushed (empty if exact version already existed).
    pub published_tags: Vec<String>,
    /// Feature version string.
    pub version: String,
}

impl FeaturePublishResult {
    fn to_json(&self) -> Value {
        json!({
            "publishedTags": self.published_tags,
            "digest": self.digest,
            "version": self.version,
        })
    }
}

/// Error type for publish operations.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error(transparent)]
    Package(#[from] PackageError),

    #[error("feature '{id}' has an invalid semver version '{version}': {reason}")]
    InvalidVersion {
        id: String,
        version: String,
        reason: String,
    },

    #[error(transparent)]
    Push(#[from] PushError),

    #[error("I/O error reading packaged artifact: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Package and publish features to the OCI registry.
///
/// Returns a map of `feature_id -> FeaturePublishResult` for each feature that
/// was attempted. The caller should JSON-serialise this to stdout.
///
/// # Errors
///
/// Returns an error if packaging fails, if a version string is not valid
/// semver, or if the OCI push fails for any feature.
pub async fn publish(
    opts: &PublishOptions,
) -> Result<HashMap<String, FeaturePublishResult>, PublishError> {
    let tmp = tempfile::tempdir().map_err(PublishError::Io)?;
    let output_folder = tmp.path().to_owned();

    let package_result = package(&PackageOptions {
        target: opts.target.clone(),
        output_folder: output_folder.clone(),
        force_clean_output_folder: false,
    })?;

    let mut results: HashMap<String, FeaturePublishResult> = HashMap::new();

    for feature in &package_result.features {
        let result =
            publish_one_feature(feature, &output_folder, &opts.registry, &opts.namespace).await?;
        results.insert(feature.id.clone(), result);
    }

    // Publish the collection index.
    publish_collection_index(&output_folder, &opts.registry, &opts.namespace).await?;

    Ok(results)
}

/// Build the JSON output map for printing to stdout.
pub fn results_to_json<S: std::hash::BuildHasher>(
    results: &HashMap<String, FeaturePublishResult, S>,
) -> Value {
    let map: serde_json::Map<String, Value> = results
        .iter()
        .map(|(id, r)| (id.clone(), r.to_json()))
        .collect();
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Per-feature publish
// ---------------------------------------------------------------------------

async fn publish_one_feature(
    feature: &PackagedFeature,
    output_folder: &std::path::Path,
    registry: &str,
    namespace: &str,
) -> Result<FeaturePublishResult, PublishError> {
    let version_str = &feature.version;
    let version = Version::parse(version_str).map_err(|e| PublishError::InvalidVersion {
        id: feature.id.clone(),
        version: version_str.clone(),
        reason: e.to_string(),
    })?;

    let repository = format!("{namespace}/{}", feature.id);
    let existing_tags = list_published_tags(registry, &repository).await?;

    // Compute which tags to push; None means exact version already published.
    let Some(tags) = compute_semver_tags(&version, &existing_tags) else {
        warn!(
            id = %feature.id,
            version = %version_str,
            "exact version already published; skipping"
        );
        eprintln!(
            "(!) Version '{version_str}' of feature '{}' already exists in the registry, skipping...",
            feature.id
        );
        return Ok(FeaturePublishResult {
            digest: String::new(),
            published_tags: Vec::new(),
            version: version_str.clone(),
        });
    };

    debug!(
        id = %feature.id,
        version = %version_str,
        ?tags,
        "publishing feature"
    );

    let archive_name = format!("devcontainer-feature-{}.tgz", feature.id);
    let archive_path = output_folder.join(&archive_name);
    let tgz_bytes = fs::read(&archive_path)?;

    let mut layer_annotations = HashMap::new();
    layer_annotations.insert(
        "org.opencontainers.image.title".to_owned(),
        archive_name.clone(),
    );

    let layer = LayerSpec {
        data: tgz_bytes,
        media_type: MEDIA_TYPE_LAYER.to_owned(),
        annotations: Some(layer_annotations),
    };

    let manifest_annotations = build_feature_annotations(feature, registry);

    let push_result = push_artifact(
        registry,
        &repository,
        &tags,
        vec![layer],
        MEDIA_TYPE_CONFIG,
        Some(manifest_annotations),
    )
    .await?;

    Ok(FeaturePublishResult {
        digest: push_result.digest,
        published_tags: push_result.pushed_tags,
        version: version_str.clone(),
    })
}

// ---------------------------------------------------------------------------
// Collection index publish
// ---------------------------------------------------------------------------

async fn publish_collection_index(
    output_folder: &std::path::Path,
    registry: &str,
    namespace: &str,
) -> Result<(), PublishError> {
    let collection_path = output_folder.join("devcontainer-collection.json");
    let collection_bytes = fs::read(&collection_path)?;

    let mut layer_annotations = HashMap::new();
    layer_annotations.insert(
        "org.opencontainers.image.title".to_owned(),
        "devcontainer-collection.json".to_owned(),
    );

    let layer = LayerSpec {
        data: collection_bytes,
        media_type: MEDIA_TYPE_COLLECTION_LAYER.to_owned(),
        annotations: Some(layer_annotations),
    };

    let mut manifest_annotations = HashMap::new();
    if registry == GHCR_IO {
        manifest_annotations.insert(
            "com.github.package.type".to_owned(),
            "devcontainer_collection".to_owned(),
        );
    }

    // Collection is always published to `namespace` (no feature id), tagged `latest`.
    let tags = vec!["latest".to_owned()];

    push_artifact(
        registry,
        namespace,
        &tags,
        vec![layer],
        MEDIA_TYPE_CONFIG,
        Some(manifest_annotations),
    )
    .await?;

    debug!("published collection index to {registry}/{namespace}:latest");
    Ok(())
}

// ---------------------------------------------------------------------------
// Semver tag fan-out
// ---------------------------------------------------------------------------

/// Compute the set of tags to publish for `version` given `existing_tags`.
///
/// Returns `None` when the exact version tag already exists (skip-republish).
/// Otherwise returns the subset of `[major, major.minor, major.minor.patch, latest]`
/// that this version should claim, based on whether it beats the current max
/// in each range — matching the official devcontainer CLI semantics.
pub fn compute_semver_tags(version: &Version, existing_tags: &[String]) -> Option<Vec<String>> {
    // Skip if the exact version tag already exists.
    if existing_tags.iter().any(|t| t == &version.to_string()) {
        return None;
    }

    let mut tags = Vec::new();

    // Exact version tag is always published.
    tags.push(version.to_string());

    // Major tag: publish if version is the new max for `X.*.*`.
    if is_new_major_max(version, existing_tags) {
        tags.push(version.major.to_string());
    }

    // Minor tag: publish if version is the new max for `X.Y.*`.
    if is_new_minor_max(version, existing_tags) {
        tags.push(format!("{}.{}", version.major, version.minor));
    }

    // `latest`: publish if version is the new global max.
    if is_new_global_max(version, existing_tags) {
        tags.push("latest".to_owned());
    }

    Some(tags)
}

/// Return `true` if `version` is strictly greater than all existing semver tags
/// with the same major version.
fn is_new_major_max(version: &Version, existing_tags: &[String]) -> bool {
    let current_max = existing_tags
        .iter()
        .filter_map(|t| Version::parse(t).ok())
        .filter(|v| v.major == version.major)
        .max();
    current_max.is_none_or(|m| version > &m)
}

/// Return `true` if `version` is strictly greater than all existing semver tags
/// with the same major.minor version.
fn is_new_minor_max(version: &Version, existing_tags: &[String]) -> bool {
    let current_max = existing_tags
        .iter()
        .filter_map(|t| Version::parse(t).ok())
        .filter(|v| v.major == version.major && v.minor == version.minor)
        .max();
    current_max.is_none_or(|m| version > &m)
}

/// Return `true` if `version` beats every existing published semver tag.
fn is_new_global_max(version: &Version, existing_tags: &[String]) -> bool {
    let current_max = existing_tags
        .iter()
        .filter_map(|t| Version::parse(t).ok())
        .max();
    current_max.is_none_or(|m| version > &m)
}

// ---------------------------------------------------------------------------
// Annotation helpers
// ---------------------------------------------------------------------------

fn build_feature_annotations(feature: &PackagedFeature, registry: &str) -> HashMap<String, String> {
    let mut annotations = HashMap::new();

    // `dev.containers.metadata` carries the full feature manifest JSON.
    let meta = Value::Object(feature.raw.clone());
    if let Ok(json_str) = serde_json::to_string(&meta) {
        annotations.insert("dev.containers.metadata".to_owned(), json_str);
    }

    // GHCR-specific package type annotation.
    if registry == GHCR_IO {
        annotations.insert(
            "com.github.package.type".to_owned(),
            "devcontainer_feature".to_owned(),
        );
    }

    annotations
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    // -----------------------------------------------------------------------
    // compute_semver_tags — basic fan-out
    // -----------------------------------------------------------------------

    #[test]
    fn first_publish_gets_all_four_tags() {
        let tags = compute_semver_tags(&ver("1.2.3"), &[]).unwrap();
        assert!(tags.contains(&"1.2.3".to_owned()), "{tags:?}");
        assert!(tags.contains(&"1".to_owned()), "{tags:?}");
        assert!(tags.contains(&"1.2".to_owned()), "{tags:?}");
        assert!(tags.contains(&"latest".to_owned()), "{tags:?}");
        assert_eq!(tags.len(), 4, "{tags:?}");
    }

    #[test]
    fn patch_that_is_new_max_updates_minor_major_and_latest() {
        let existing = vec![
            "1.2.3".to_owned(),
            "1.2".to_owned(),
            "1".to_owned(),
            "latest".to_owned(),
        ];
        let tags = compute_semver_tags(&ver("1.2.4"), &existing).unwrap();
        assert!(tags.contains(&"1.2.4".to_owned()), "{tags:?}");
        assert!(tags.contains(&"1.2".to_owned()), "{tags:?}");
        assert!(tags.contains(&"1".to_owned()), "{tags:?}");
        assert!(tags.contains(&"latest".to_owned()), "{tags:?}");
    }

    #[test]
    fn patch_on_older_minor_does_not_steal_major_or_latest() {
        // 1.2.x is already at 1.2.3; 2.0.0 is the global max.
        let existing = vec![
            "1.2.3".to_owned(),
            "1.2".to_owned(),
            "2.0.0".to_owned(),
            "2".to_owned(),
            "2.0".to_owned(),
            "latest".to_owned(),
        ];
        // Publishing 1.2.4 — beats 1.2.3 but not 2.0.0.
        let tags = compute_semver_tags(&ver("1.2.4"), &existing).unwrap();
        assert!(tags.contains(&"1.2.4".to_owned()), "{tags:?}");
        assert!(tags.contains(&"1.2".to_owned()), "{tags:?}"); // new max for 1.2.x
        // Should NOT claim major "1" (no existing 1.x.x to beat is present, so it
        // would get "1") — actually: there's no existing 1.x tag, so 1.2.4 IS the
        // max for 1.x.x range and should claim "1".
        assert!(tags.contains(&"1".to_owned()), "{tags:?}");
        // Should NOT claim "latest" (2.0.0 > 1.2.4).
        assert!(!tags.contains(&"latest".to_owned()), "{tags:?}");
    }

    #[test]
    fn new_major_claims_major_minor_exact_and_latest() {
        let existing = vec![
            "1.0.0".to_owned(),
            "1.0".to_owned(),
            "1".to_owned(),
            "latest".to_owned(),
        ];
        let tags = compute_semver_tags(&ver("2.0.0"), &existing).unwrap();
        assert!(tags.contains(&"2.0.0".to_owned()), "{tags:?}");
        assert!(tags.contains(&"2".to_owned()), "{tags:?}");
        assert!(tags.contains(&"2.0".to_owned()), "{tags:?}");
        assert!(tags.contains(&"latest".to_owned()), "{tags:?}");
    }

    // -----------------------------------------------------------------------
    // compute_semver_tags — skip-republish
    // -----------------------------------------------------------------------

    #[test]
    fn exact_version_already_exists_returns_none() {
        let existing = vec!["1.2.3".to_owned(), "1.2".to_owned(), "latest".to_owned()];
        assert!(compute_semver_tags(&ver("1.2.3"), &existing).is_none());
    }

    // -----------------------------------------------------------------------
    // is_new_major_max / is_new_minor_max / is_new_global_max
    // -----------------------------------------------------------------------

    #[test]
    fn major_max_ignores_non_semver_tags() {
        let tags: Vec<String> = vec![
            "1.2.3".to_owned(),
            "latest".to_owned(), // not valid semver — ignored
            "1.2".to_owned(),    // not valid semver (missing patch) — ignored
            "not-a-version".to_owned(),
        ];
        // 1.2.3 is already the max for major=1; 1.2.4 beats it.
        assert!(is_new_major_max(&ver("1.2.4"), &tags));
        assert!(!is_new_major_max(&ver("1.2.2"), &tags));
    }

    #[test]
    fn major_max_returns_true_for_empty_tags() {
        assert!(is_new_major_max(&ver("1.0.0"), &[]));
    }

    #[test]
    fn minor_max_ignores_non_semver_tags() {
        let tags: Vec<String> = vec!["1.2.3".to_owned(), "latest".to_owned()];
        assert!(is_new_minor_max(&ver("1.2.4"), &tags));
        assert!(!is_new_minor_max(&ver("1.2.2"), &tags));
    }

    #[test]
    fn global_max_returns_true_for_empty_tags() {
        assert!(is_new_global_max(&ver("1.0.0"), &[]));
    }

    // -----------------------------------------------------------------------
    // build_feature_annotations
    // -----------------------------------------------------------------------

    #[test]
    fn ghcr_annotations_include_package_type() {
        let feature = PackagedFeature {
            id: "my-feature".to_owned(),
            version: "1.0.0".to_owned(),
            name: Some("My Feature".to_owned()),
            description: None,
            raw: serde_json::Map::new(),
        };
        let ann = build_feature_annotations(&feature, "ghcr.io");
        assert_eq!(
            ann.get("com.github.package.type").map(String::as_str),
            Some("devcontainer_feature")
        );
        assert!(ann.contains_key("dev.containers.metadata"));
    }

    #[test]
    fn non_ghcr_annotations_omit_package_type() {
        let feature = PackagedFeature {
            id: "my-feature".to_owned(),
            version: "1.0.0".to_owned(),
            name: None,
            description: None,
            raw: serde_json::Map::new(),
        };
        let ann = build_feature_annotations(&feature, "myregistry.example.com");
        assert!(!ann.contains_key("com.github.package.type"));
    }
}
