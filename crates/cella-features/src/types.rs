//! Core types shared across the cella-features crate.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Target platform for multi-arch feature resolution.
#[derive(Debug, Clone)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

/// A resolved feature ready for installation.
#[derive(Debug, Clone)]
pub struct ResolvedFeature {
    pub id: String,
    pub original_ref: String,
    pub metadata: FeatureMetadata,
    pub user_options: HashMap<String, serde_json::Value>,
    pub artifact_dir: PathBuf,
    pub has_install_script: bool,
    /// OCI source artifact (resolved manifest digest + the manifest blob),
    /// present only for OCI-fetched features. `None` for local/tarball.
    ///
    /// Retained for `read-configuration --include-features-configuration`:
    /// `sourceInformation.manifestDigest` and `sourceInformation.manifest`. The
    /// `featureRef` parts are parsed from `original_ref` (matching the official
    /// `getRef`), so only the resolved digest + manifest blob need carrying.
    pub oci: Option<ResolvedOciManifest>,
}

/// The resolved OCI artifact for a feature: its manifest digest and blob.
///
/// Both travel together — an OCI feature always has both — so they are bundled
/// rather than two parallel `Option`s that could disagree. Retained for
/// `read-configuration`'s `sourceInformation.{manifestDigest,manifest}`.
#[derive(Debug, Clone)]
pub struct ResolvedOciManifest {
    /// The normalized OCI coordinates actually fetched, after alias/redirect
    /// resolution (e.g. `maven` → `ghcr.io/devcontainers/features/java`). Used
    /// to build `sourceInformation.featureRef` — which the official derives from
    /// the fetched identifier, not the raw user ref.
    pub registry: String,
    pub repository: String,
    pub version: String,
    /// The resolved manifest content digest (`sourceInformation.manifestDigest`,
    /// e.g. `"sha256:abc…"`).
    pub digest: String,
    /// The manifest blob (`sourceInformation.manifest`).
    pub manifest: oci_distribution::manifest::OciImageManifest,
}

/// Parsed devcontainer-feature.json.
#[derive(Debug, Clone, Default)]
pub struct FeatureMetadata {
    pub id: String,
    pub version: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub documentation_url: Option<String>,
    pub options: HashMap<String, FeatureOption>,
    /// Hard dependencies declared via `dependsOn` in `devcontainer-feature.json`.
    ///
    /// Keys are feature reference strings; values are the options map to pass to
    /// that feature (arbitrary JSON — `string | boolean | object` per spec).
    ///
    /// A `BTreeMap` keeps iteration order stable so dependency-graph rendering
    /// (e.g. the Mermaid diagram) is deterministic across runs.
    pub depends_on: BTreeMap<String, serde_json::Value>,
    /// Soft ordering hints declared via `installsAfter`.
    pub installs_after: Vec<String>,
    pub container_user: Option<String>,
    pub entrypoint: Option<String>,
    pub mounts: Vec<String>,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub privileged: Option<bool>,
    pub init: Option<bool>,
    pub container_env: HashMap<String, String>,
    pub customizations: Option<serde_json::Value>,
    pub on_create_command: Option<serde_json::Value>,
    pub update_content_command: Option<serde_json::Value>,
    pub post_create_command: Option<serde_json::Value>,
    pub post_start_command: Option<serde_json::Value>,
    pub post_attach_command: Option<serde_json::Value>,
    pub legacy_ids: Vec<String>,
    /// Canonical id injected at packaging time, present in fetched metadata only
    /// when `legacyIds` is non-empty.  Used to detect rename warnings: when the
    /// user's ref leaf differs from `current_id`, the feature was referenced by a
    /// legacy id and we emit `(!) WARNING: This feature has been renamed…`.
    pub current_id: Option<String>,
    pub deprecated: Option<bool>,
}

/// A feature option declaration.
#[derive(Debug, Clone)]
pub struct FeatureOption {
    pub option_type: OptionType,
    pub default: serde_json::Value,
    pub description: Option<String>,
    pub enum_values: Option<Vec<String>>,
}

/// Option type variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionType {
    String,
    Boolean,
}

/// A lifecycle command paired with its origin (feature id or "devcontainer.json").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEntry {
    pub origin: String,
    pub command: serde_json::Value,
}

/// Merged container configuration from all features.
#[derive(Debug, Clone, Default)]
pub struct FeatureContainerConfig {
    pub mounts: Vec<String>,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub privileged: bool,
    pub init: bool,
    pub container_env: HashMap<String, String>,
    pub entrypoints: Vec<String>,
    pub lifecycle: FeatureLifecycle,
    pub customizations: serde_json::Value,
}

/// Feature lifecycle commands collected from all features.
#[derive(Debug, Clone, Default)]
pub struct FeatureLifecycle {
    pub on_create: Vec<LifecycleEntry>,
    pub update_content: Vec<LifecycleEntry>,
    pub post_create: Vec<LifecycleEntry>,
    pub post_start: Vec<LifecycleEntry>,
    pub post_attach: Vec<LifecycleEntry>,
}

/// Resolved features output -- everything needed to build and configure the container.
#[derive(Debug)]
pub struct ResolvedFeatures {
    pub features: Vec<ResolvedFeature>,
    pub dockerfile: String,
    pub build_context: PathBuf,
    pub container_config: FeatureContainerConfig,
    pub metadata_label: String,
    /// The lockfile written or validated during feature resolution, if any.
    pub lockfile: Option<crate::lockfile::Lockfile>,
}
