//! Core types shared across the cella-features crate.

use std::collections::HashMap;
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
}

/// Parsed devcontainer-feature.json.
#[derive(Debug, Clone, Default)]
pub struct FeatureMetadata {
    pub id: String,
    pub version: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub options: HashMap<String, FeatureOption>,
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
    pub post_create_command: Option<serde_json::Value>,
    pub post_start_command: Option<serde_json::Value>,
    pub post_attach_command: Option<serde_json::Value>,
    pub legacy_ids: Vec<String>,
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
    pub on_create: Vec<serde_json::Value>,
    pub post_create: Vec<serde_json::Value>,
    pub post_start: Vec<serde_json::Value>,
    pub post_attach: Vec<serde_json::Value>,
}

/// Resolved features output -- everything needed to build and configure the container.
#[derive(Debug)]
pub struct ResolvedFeatures {
    pub features: Vec<ResolvedFeature>,
    pub dockerfile: String,
    pub build_context: PathBuf,
    pub container_config: FeatureContainerConfig,
    pub metadata_label: String,
}
