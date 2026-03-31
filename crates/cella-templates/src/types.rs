//! Core data types for devcontainer templates.
//!
//! These types model the devcontainer template specification: collection
//! indexes, template metadata, options, and user selections.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Collection index (fetched from OCI registry)
// ---------------------------------------------------------------------------

/// The collection index fetched from a registry's
/// `devcontainer-collection.json` artifact.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateCollectionIndex {
    /// All templates available in this collection.
    #[serde(default)]
    pub templates: Vec<TemplateSummary>,

    /// Opaque metadata from the packaging tool.
    #[serde(default)]
    pub source_information: Option<serde_json::Value>,
}

/// A single template entry from the collection index.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSummary {
    /// Unique identifier (must match the OCI artifact name).
    pub id: String,

    /// Semantic version.
    pub version: String,

    /// Human-readable display name.
    #[serde(default)]
    pub name: Option<String>,

    /// Short description.
    #[serde(default)]
    pub description: Option<String>,

    /// Platform categories (e.g. `["Rust"]`, `["Any"]`).
    #[serde(default)]
    pub platforms: Vec<String>,

    /// Search keywords.
    #[serde(default)]
    pub keywords: Vec<String>,
}

// ---------------------------------------------------------------------------
// Feature collection index
// ---------------------------------------------------------------------------

/// The feature collection index from `devcontainer-collection.json`.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureCollectionIndex {
    /// All features available in this collection.
    #[serde(default)]
    pub features: Vec<FeatureSummary>,

    /// Opaque metadata from the packaging tool.
    #[serde(default)]
    pub source_information: Option<serde_json::Value>,
}

/// A single feature entry from the collection index.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureSummary {
    /// Unique identifier.
    pub id: String,

    /// Semantic version.
    pub version: String,

    /// Human-readable display name.
    #[serde(default)]
    pub name: Option<String>,

    /// Short description.
    #[serde(default)]
    pub description: Option<String>,

    /// Search keywords.
    #[serde(default)]
    pub keywords: Vec<String>,
}

// ---------------------------------------------------------------------------
// Template metadata (from devcontainer-template.json inside the artifact)
// ---------------------------------------------------------------------------

/// Full template metadata parsed from `devcontainer-template.json`.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateMetadata {
    /// Unique identifier (must match the directory/artifact name).
    pub id: String,

    /// Semantic version.
    pub version: String,

    /// Human-readable display name.
    #[serde(default)]
    pub name: Option<String>,

    /// Short description.
    #[serde(default)]
    pub description: Option<String>,

    /// URL to documentation.
    #[serde(default)]
    pub documentation_url: Option<String>,

    /// URL to license.
    #[serde(default)]
    pub license_url: Option<String>,

    /// Publisher/maintainer name.
    #[serde(default)]
    pub publisher: Option<String>,

    /// Platform categories.
    #[serde(default)]
    pub platforms: Vec<String>,

    /// Search keywords.
    #[serde(default)]
    pub keywords: Vec<String>,

    /// Paths that may be excluded from the template (e.g. `[".github/*"]`).
    #[serde(default)]
    pub optional_paths: Vec<String>,

    /// Configurable options for this template.
    #[serde(default)]
    pub options: HashMap<String, TemplateOption>,
}

/// A single template option declaration.
///
/// Options use either `proposals` (flexible, free-form input allowed) or
/// `enum` (strict, only listed values allowed), never both.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct TemplateOption {
    /// The option type: `"string"` or `"boolean"`.
    #[serde(rename = "type")]
    pub option_type: String,

    /// Human-readable description shown to the user.
    #[serde(default)]
    pub description: Option<String>,

    /// Default value for this option.
    pub default: serde_json::Value,

    /// Suggested values (user can still enter a custom value).
    #[serde(default)]
    pub proposals: Option<Vec<String>>,

    /// Strictly allowed values (user must choose from this list).
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// User selections (output of wizard / CLI flags)
// ---------------------------------------------------------------------------

/// The user's complete init selections, ready to be applied.
#[derive(Debug, Clone)]
pub struct InitSelection {
    /// OCI reference of the chosen template.
    pub template_ref: String,

    /// Values for template options (key → value).
    pub template_options: HashMap<String, serde_json::Value>,

    /// Features selected by the user.
    pub features: Vec<SelectedFeature>,

    /// Whether to emit JSONC (with comments) or plain JSON.
    pub output_format: OutputFormat,

    /// Path where the configuration will be written.
    pub output_path: PathBuf,
}

/// A single feature selected by the user, with its configured options.
#[derive(Debug, Clone)]
pub struct SelectedFeature {
    /// Full OCI reference (e.g. `ghcr.io/devcontainers/features/node:1`).
    pub reference: String,

    /// Option values chosen by the user (key → value).
    pub options: HashMap<String, serde_json::Value>,
}

/// Output format for the generated devcontainer configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// JSONC with section comments.
    Jsonc,
    /// Plain JSON (no comments).
    Json,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // TemplateCollectionIndex deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn deserialize_minimal_collection() {
        let json = r#"{ "templates": [] }"#;
        let index: TemplateCollectionIndex = serde_json::from_str(json).unwrap();
        assert!(index.templates.is_empty());
        assert!(index.source_information.is_none());
    }

    #[test]
    fn deserialize_collection_with_templates() {
        let json = r#"{
            "sourceInformation": { "tool": "devcontainers-cli" },
            "templates": [
                {
                    "id": "rust",
                    "version": "5.0.0",
                    "name": "Rust",
                    "description": "Develop Rust applications.",
                    "platforms": ["Rust"],
                    "keywords": ["rust", "cargo"]
                },
                {
                    "id": "debian",
                    "version": "4.0.0",
                    "name": "Debian",
                    "description": "Simple Debian container.",
                    "platforms": ["Any"],
                    "keywords": []
                }
            ]
        }"#;
        let index: TemplateCollectionIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.templates.len(), 2);
        assert_eq!(index.templates[0].id, "rust");
        assert_eq!(index.templates[0].version, "5.0.0");
        assert_eq!(index.templates[0].name.as_deref(), Some("Rust"));
        assert_eq!(index.templates[0].platforms, vec!["Rust"]);
        assert_eq!(index.templates[1].id, "debian");
        assert!(index.source_information.is_some());
    }

    #[test]
    fn deserialize_collection_missing_optional_fields() {
        let json = r#"{
            "templates": [
                { "id": "minimal", "version": "1.0.0" }
            ]
        }"#;
        let index: TemplateCollectionIndex = serde_json::from_str(json).unwrap();
        let t = &index.templates[0];
        assert_eq!(t.id, "minimal");
        assert!(t.name.is_none());
        assert!(t.description.is_none());
        assert!(t.platforms.is_empty());
        assert!(t.keywords.is_empty());
    }

    // -----------------------------------------------------------------------
    // FeatureCollectionIndex deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn deserialize_feature_collection() {
        let json = r#"{
            "features": [
                {
                    "id": "node",
                    "version": "1.5.0",
                    "name": "Node.js",
                    "description": "Installs Node.js and npm",
                    "keywords": ["node", "npm", "javascript"]
                }
            ]
        }"#;
        let index: FeatureCollectionIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.features.len(), 1);
        assert_eq!(index.features[0].id, "node");
        assert_eq!(index.features[0].name.as_deref(), Some("Node.js"));
    }

    // -----------------------------------------------------------------------
    // TemplateMetadata deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn deserialize_template_metadata_full() {
        let json = r#"{
            "id": "rust",
            "version": "5.0.0",
            "name": "Rust",
            "description": "Develop Rust based applications.",
            "documentationURL": "https://github.com/devcontainers/templates/tree/main/src/rust",
            "publisher": "Dev Container Spec Maintainers",
            "licenseURL": "https://github.com/devcontainers/templates/blob/main/LICENSE",
            "platforms": ["Rust"],
            "optionalPaths": [".github/*"],
            "options": {
                "imageVariant": {
                    "type": "string",
                    "description": "Debian OS version (trixie, bookworm, bullseye):",
                    "proposals": ["trixie", "bookworm", "bullseye"],
                    "default": "trixie"
                }
            }
        }"#;
        let meta: TemplateMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.id, "rust");
        assert_eq!(meta.version, "5.0.0");
        assert_eq!(meta.name.as_deref(), Some("Rust"));
        assert_eq!(
            meta.publisher.as_deref(),
            Some("Dev Container Spec Maintainers")
        );
        assert_eq!(meta.optional_paths, vec![".github/*"]);
        assert_eq!(meta.options.len(), 1);

        let opt = &meta.options["imageVariant"];
        assert_eq!(opt.option_type, "string");
        assert_eq!(opt.default, serde_json::json!("trixie"));
        assert_eq!(opt.proposals.as_ref().unwrap().len(), 3);
        assert!(opt.enum_values.is_none());
    }

    #[test]
    fn deserialize_template_metadata_with_boolean_option() {
        let json = r#"{
            "id": "java",
            "version": "5.0.0",
            "name": "Java",
            "options": {
                "imageVariant": {
                    "type": "string",
                    "proposals": ["25-trixie", "21-trixie"],
                    "default": "25-trixie"
                },
                "installMaven": {
                    "type": "boolean",
                    "description": "Install Maven for Java build automation",
                    "default": false
                },
                "installGradle": {
                    "type": "boolean",
                    "description": "Install Gradle for Java build automation",
                    "default": false
                }
            }
        }"#;
        let meta: TemplateMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.options.len(), 3);

        let maven = &meta.options["installMaven"];
        assert_eq!(maven.option_type, "boolean");
        assert_eq!(maven.default, serde_json::json!(false));
        assert!(maven.proposals.is_none());
        assert!(maven.enum_values.is_none());
    }

    #[test]
    fn deserialize_template_metadata_with_enum_option() {
        let json = r#"{
            "id": "test",
            "version": "1.0.0",
            "options": {
                "variant": {
                    "type": "string",
                    "description": "Pick a variant",
                    "enum": ["a", "b", "c"],
                    "default": "a"
                }
            }
        }"#;
        let meta: TemplateMetadata = serde_json::from_str(json).unwrap();
        let opt = &meta.options["variant"];
        assert_eq!(opt.enum_values.as_ref().unwrap(), &["a", "b", "c"]);
        assert!(opt.proposals.is_none());
    }

    #[test]
    fn deserialize_template_metadata_minimal() {
        let json = r#"{ "id": "bare", "version": "0.1.0" }"#;
        let meta: TemplateMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.id, "bare");
        assert!(meta.name.is_none());
        assert!(meta.options.is_empty());
        assert!(meta.optional_paths.is_empty());
    }
}
