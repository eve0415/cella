//! JSONC comment-preserving edit layer for devcontainer.json features.
//!
//! Uses `jsonc-parser`'s CST API to modify the `"features"` object in a
//! devcontainer.json file while preserving all comments and formatting.

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};

/// Represents an edit operation on features in devcontainer.json.
#[derive(Debug, Clone)]
pub enum FeatureEdit {
    /// Add a feature with its options object.
    Add {
        reference: String,
        options: serde_json::Value,
    },
    /// Remove a feature by its reference key.
    Remove { reference: String },
    /// Set a single option on an existing feature.
    SetOption {
        reference: String,
        key: String,
        value: serde_json::Value,
    },
    /// Replace the entire options object for a feature.
    ReplaceOptions {
        reference: String,
        options: serde_json::Value,
    },
}

/// Apply a batch of edits to JSONC source, preserving comments and formatting.
///
/// # Errors
///
/// Returns error if the JSONC cannot be parsed or edits target invalid paths.
pub fn apply_edits(
    source: &str,
    edits: &[FeatureEdit],
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    if edits.is_empty() {
        return Ok(source.to_owned());
    }

    let root = CstRootNode::parse(source, &ParseOptions::default())
        .map_err(|e| format!("failed to parse JSONC: {e}"))?;
    let root_obj = root.object_value_or_set();

    for edit in edits {
        match edit {
            FeatureEdit::Add { reference, options } => {
                let features_obj = ensure_features_object(&root_obj);
                features_obj.append(reference, to_cst_value(options));
            }
            FeatureEdit::Remove { reference } => {
                if let Some(features_prop) = root_obj.get("features")
                    && let Some(features_obj) = features_prop.object_value()
                {
                    if let Some(prop) = features_obj.get(reference) {
                        prop.remove();
                    }
                    // If features object is now empty, remove the whole key
                    if features_obj.properties().is_empty() {
                        features_prop.remove();
                    }
                }
            }
            FeatureEdit::SetOption {
                reference,
                key,
                value,
            } => {
                if let Some(features_prop) = root_obj.get("features")
                    && let Some(features_obj) = features_prop.object_value()
                    && let Some(feature_prop) = features_obj.get(reference)
                {
                    let feature_obj = feature_prop.object_value_or_set();
                    if let Some(existing) = feature_obj.get(key) {
                        existing.set_value(to_cst_value(value));
                    } else {
                        feature_obj.append(key, to_cst_value(value));
                    }
                }
            }
            FeatureEdit::ReplaceOptions { reference, options } => {
                if let Some(features_prop) = root_obj.get("features")
                    && let Some(features_obj) = features_prop.object_value()
                    && let Some(feature_prop) = features_obj.get(reference)
                {
                    feature_prop.set_value(to_cst_value(options));
                }
            }
        }
    }

    Ok(root.to_string())
}

/// Ensure the root object has a `"features"` property with an object value.
/// Returns the features object (creating it if needed).
fn ensure_features_object(root_obj: &jsonc_parser::cst::CstObject) -> jsonc_parser::cst::CstObject {
    if let Some(prop) = root_obj.get("features")
        && let Some(obj) = prop.object_value()
    {
        return obj;
    }
    root_obj.append("features", CstInputValue::Object(vec![]));
    root_obj
        .get("features")
        .expect("just appended features")
        .object_value()
        .expect("just set to object")
}

/// Convert a `serde_json::Value` to a `jsonc_parser::cst::CstInputValue`.
fn to_cst_value(value: &serde_json::Value) -> CstInputValue {
    match value {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(b) => CstInputValue::Bool(*b),
        serde_json::Value::Number(n) => CstInputValue::Number(n.to_string()),
        serde_json::Value::String(s) => CstInputValue::String(s.clone()),
        serde_json::Value::Array(arr) => {
            CstInputValue::Array(arr.iter().map(to_cst_value).collect())
        }
        serde_json::Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), to_cst_value(v)))
                .collect(),
        ),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_feature_to_empty_config() {
        let source = r#"{ "name": "Test" }"#;
        let edits = vec![FeatureEdit::Add {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            options: serde_json::json!({}),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("\"features\""));
        assert!(result.contains("ghcr.io/devcontainers/features/node:1"));
    }

    #[test]
    fn add_feature_to_existing_features() {
        let source = r#"{
  "name": "Test",
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}
  }
}"#;
        let edits = vec![FeatureEdit::Add {
            reference: "ghcr.io/devcontainers/features/python:1".to_owned(),
            options: serde_json::json!({"version": "3.12"}),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("node:1"));
        assert!(result.contains("python:1"));
        assert!(result.contains("\"version\""));
    }

    #[test]
    fn remove_feature() {
        let source = r#"{
  "name": "Test",
  "features": {
    "ghcr.io/devcontainers/features/node:1": {},
    "ghcr.io/devcontainers/features/python:1": {}
  }
}"#;
        let edits = vec![FeatureEdit::Remove {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(!result.contains("node:1"));
        assert!(result.contains("python:1"));
        assert!(result.contains("\"features\""));
    }

    #[test]
    fn remove_last_feature_removes_key() {
        let source = r#"{
  "name": "Test",
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}
  }
}"#;
        let edits = vec![FeatureEdit::Remove {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(!result.contains("node:1"));
        assert!(!result.contains("\"features\""));
    }

    #[test]
    fn set_option_on_existing_feature() {
        let source = r#"{
  "features": {
    "ghcr.io/devcontainers/features/node:1": {
      "version": "lts"
    }
  }
}"#;
        let edits = vec![FeatureEdit::SetOption {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            key: "version".to_owned(),
            value: serde_json::json!("20"),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("\"20\""));
        assert!(!result.contains("\"lts\""));
    }

    #[test]
    fn set_new_option_on_feature() {
        let source = r#"{
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}
  }
}"#;
        let edits = vec![FeatureEdit::SetOption {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            key: "version".to_owned(),
            value: serde_json::json!("lts"),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("\"version\""));
        assert!(result.contains("\"lts\""));
    }

    #[test]
    fn replace_options() {
        let source = r#"{
  "features": {
    "ghcr.io/devcontainers/features/node:1": {
      "version": "lts",
      "pnpmVersion": "latest"
    }
  }
}"#;
        let edits = vec![FeatureEdit::ReplaceOptions {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            options: serde_json::json!({"version": "20"}),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("\"20\""));
        assert!(!result.contains("pnpmVersion"));
    }

    #[test]
    fn multiple_edits_in_batch() {
        let source = r#"{
  "name": "Test",
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}
  }
}"#;
        let edits = vec![
            FeatureEdit::Remove {
                reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            },
            FeatureEdit::Add {
                reference: "ghcr.io/devcontainers/features/python:1".to_owned(),
                options: serde_json::json!({"version": "3.12"}),
            },
        ];
        let result = apply_edits(source, &edits).unwrap();
        assert!(!result.contains("node:1"));
        assert!(result.contains("python:1"));
    }

    #[test]
    fn preserves_comments() {
        let source = r#"{
  // This is a name comment
  "name": "Test",
  // Features comment
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}
  }
}"#;
        let edits = vec![FeatureEdit::Add {
            reference: "ghcr.io/devcontainers/features/python:1".to_owned(),
            options: serde_json::json!({}),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("// This is a name comment"));
        assert!(result.contains("// Features comment"));
    }

    #[test]
    fn empty_edits_returns_original() {
        let source = r#"{ "name": "Test" }"#;
        let result = apply_edits(source, &[]).unwrap();
        assert_eq!(result, source);
    }

    #[test]
    fn add_feature_with_options() {
        let source = r#"{ "name": "Test" }"#;
        let edits = vec![FeatureEdit::Add {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            options: serde_json::json!({"version": "lts", "nodeGypDependencies": true}),
        }];
        let result = apply_edits(source, &edits).unwrap();
        assert!(result.contains("\"version\""));
        assert!(result.contains("\"lts\""));
        assert!(result.contains("nodeGypDependencies"));
    }
}
