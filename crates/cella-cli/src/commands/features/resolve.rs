//! Config discovery and feature resolution helpers shared across
//! `cella features` subcommands.

use std::path::{Path, PathBuf};

use clap::Args;

use cella_templates::cache::TemplateCache;
use cella_templates::fetcher;

/// Common flags shared by all features subcommands.
#[derive(Args, Clone)]
pub struct CommonFeatureFlags {
    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Workspace folder path (defaults to current directory).
    #[arg(short = 'w', long)]
    pub workspace_folder: Option<PathBuf>,

    /// OCI registry for feature collection.
    #[arg(long)]
    pub registry: Option<String>,
}

/// Discover the devcontainer.json path from flags or auto-discovery.
///
/// # Errors
///
/// Returns error with an init suggestion when no config is found.
pub fn discover_config(
    flags: &CommonFeatureFlags,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let workspace = crate::commands::resolve_workspace_folder(flags.workspace_folder.as_deref())?;

    if let Some(file) = &flags.file {
        if !file.exists() {
            return Err(format!(
                "config file not found: {}\nhint: run 'cella init' to create one",
                file.display()
            )
            .into());
        }
        return Ok(file.clone());
    }

    cella_config::devcontainer::discover::config(&workspace).map_err(|e| {
        if matches!(e, cella_config::devcontainer::discover::Error::NotFound) {
            format!("{e}\nhint: run 'cella init' to create a devcontainer configuration").into()
        } else {
            Box::new(e) as Box<dyn std::error::Error + Send + Sync>
        }
    })
}

/// Read raw file content from a config path.
///
/// # Errors
///
/// Returns I/O error.
pub fn read_raw_config(path: &Path) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    Ok(std::fs::read_to_string(path)?)
}

/// Extract features from a parsed JSON config.
///
/// Returns pairs of `(reference, options_value)` preserving insertion order.
pub fn extract_features(config: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let Some(features) = config.get("features").and_then(|f| f.as_object()) else {
        return Vec::new();
    };
    features
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Match a short ID like `"node"` to a full OCI reference from a list.
///
/// Tries exact match first, then falls back to matching the last path
/// segment before the tag (e.g. `ghcr.io/.../node:1` matches `"node"`).
///
/// Returns `None` if no match, or the full reference string.
pub fn match_feature_ref<'a>(
    short_id: &str,
    refs: &'a [(String, serde_json::Value)],
) -> Option<&'a str> {
    // Exact match
    if let Some((full_ref, _)) = refs.iter().find(|(r, _)| r == short_id) {
        return Some(full_ref);
    }
    // Short ID match: last segment before colon
    refs.iter()
        .find(|(r, _)| {
            r.rsplit('/')
                .next()
                .and_then(|s| s.split(':').next())
                .is_some_and(|id| id == short_id)
        })
        .map(|(r, _)| r.as_str())
}

/// Resolve a human-readable name for a feature reference.
///
/// Fetches `devcontainer-feature.json` from the cache/registry to get
/// the display name. Falls back to the raw reference on failure.
pub async fn resolve_feature_name(reference: &str, cache: &TemplateCache) -> String {
    if let Ok(feature_dir) = fetcher::fetch_template(reference, cache).await
        && let Ok(content) = std::fs::read_to_string(feature_dir.join("devcontainer-feature.json"))
        && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(name) = meta.get("name").and_then(|n| n.as_str())
    {
        return name.to_owned();
    }
    // Fallback: extract short ID from reference
    reference.rsplit('/').next().unwrap_or(reference).to_owned()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_features() -> Vec<(String, serde_json::Value)> {
        vec![
            (
                "ghcr.io/devcontainers/features/node:1".to_owned(),
                serde_json::json!({"version": "lts"}),
            ),
            (
                "ghcr.io/devcontainers/features/python:1".to_owned(),
                serde_json::json!({}),
            ),
            (
                "ghcr.io/devcontainers/features/go:1".to_owned(),
                serde_json::json!({"version": "1.22"}),
            ),
        ]
    }

    #[test]
    fn match_exact_reference() {
        let features = sample_features();
        let result = match_feature_ref("ghcr.io/devcontainers/features/node:1", &features);
        assert_eq!(result, Some("ghcr.io/devcontainers/features/node:1"));
    }

    #[test]
    fn match_short_id() {
        let features = sample_features();
        let result = match_feature_ref("node", &features);
        assert_eq!(result, Some("ghcr.io/devcontainers/features/node:1"));
    }

    #[test]
    fn match_short_id_python() {
        let features = sample_features();
        let result = match_feature_ref("python", &features);
        assert_eq!(result, Some("ghcr.io/devcontainers/features/python:1"));
    }

    #[test]
    fn no_match_returns_none() {
        let features = sample_features();
        let result = match_feature_ref("rust", &features);
        assert!(result.is_none());
    }

    #[test]
    fn extract_features_from_config() {
        let config = serde_json::json!({
            "name": "Test",
            "features": {
                "ghcr.io/devcontainers/features/node:1": {"version": "lts"},
                "ghcr.io/devcontainers/features/python:1": {}
            }
        });
        let features = extract_features(&config);
        assert_eq!(features.len(), 2);
    }

    #[test]
    fn extract_features_empty() {
        let config = serde_json::json!({"name": "Test"});
        let features = extract_features(&config);
        assert!(features.is_empty());
    }

    #[test]
    fn extract_features_empty_object() {
        let config = serde_json::json!({"name": "Test", "features": {}});
        let features = extract_features(&config);
        assert!(features.is_empty());
    }

    #[test]
    fn discover_config_missing_file_error() {
        let flags = CommonFeatureFlags {
            file: Some(PathBuf::from("/nonexistent/devcontainer.json")),
            workspace_folder: None,
            registry: None,
        };
        let err = discover_config(&flags).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("cella init"));
    }

    #[test]
    fn match_feature_ref_empty_list() {
        let features: Vec<(String, serde_json::Value)> = vec![];
        assert!(match_feature_ref("node", &features).is_none());
    }

    #[test]
    fn extract_features_non_object_features() {
        // features is an array instead of object — should return empty
        let config = serde_json::json!({"features": ["not", "an", "object"]});
        let features = extract_features(&config);
        assert!(features.is_empty());
    }

    #[test]
    fn extract_features_features_is_null() {
        let config = serde_json::json!({"features": null});
        let features = extract_features(&config);
        assert!(features.is_empty());
    }

    #[test]
    fn match_feature_ref_prefers_exact_over_short() {
        let features = vec![
            ("node".to_owned(), serde_json::json!({})),
            (
                "ghcr.io/devcontainers/features/node:1".to_owned(),
                serde_json::json!({}),
            ),
        ];
        // Exact match should win
        let result = match_feature_ref("node", &features);
        assert_eq!(result, Some("node"));
    }
}
