//! Template application: extract files, substitute placeholders, generate config.
//!
//! Handles the full pipeline from an extracted template artifact to a written
//! `.devcontainer/` directory with JSONC or JSON output.

use std::collections::HashMap;
use std::path::Path;

use crate::error::TemplateError;
use crate::types::{OutputFormat, SelectedFeature};

// ---------------------------------------------------------------------------
// JSONC stripping
// ---------------------------------------------------------------------------

/// Strip JSONC comments and trailing commas, mapping errors to [`TemplateError`].
fn strip_jsonc(content: &str, file_name: &str) -> Result<String, TemplateError> {
    cella_jsonc::strip(content).map_err(|e| TemplateError::InvalidArtifact {
        template_id: file_name.to_owned(),
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Template option substitution
// ---------------------------------------------------------------------------

/// Substitute `${templateOption:key}` placeholders in content.
///
/// Replaces all occurrences of `${templateOption:<key>}` with the
/// corresponding value from the options map.
pub fn substitute_template_options<S: std::hash::BuildHasher>(
    content: &str,
    options: &HashMap<String, serde_json::Value, S>,
) -> String {
    let mut result = content.to_owned();
    for (key, value) in options {
        let placeholder = format!("${{templateOption:{key}}}");
        let replacement = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };
        result = result.replace(&placeholder, &replacement);
    }
    result
}

// ---------------------------------------------------------------------------
// Feature merging
// ---------------------------------------------------------------------------

/// Merge selected features into a devcontainer config JSON value.
pub fn merge_features(config: &mut serde_json::Value, features: &[SelectedFeature]) {
    if features.is_empty() {
        return;
    }

    let features_map: serde_json::Map<String, serde_json::Value> = features
        .iter()
        .map(|f| {
            let options_value = if f.options.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::Value::Object(
                    f.options
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                )
            };
            (f.reference.clone(), options_value)
        })
        .collect();

    if let Some(obj) = config.as_object_mut() {
        obj.insert(
            "features".to_owned(),
            serde_json::Value::Object(features_map),
        );
    }
}

// ---------------------------------------------------------------------------
// JSONC generation
// ---------------------------------------------------------------------------

/// Known devcontainer.json keys and their section comments.
const SECTION_COMMENTS: &[(&str, &str)] = &[
    ("name", "A name for the dev container"),
    ("image", "The base image to use"),
    ("build", "Build configuration"),
    ("dockerFile", "Dockerfile path"),
    ("dockerComposeFile", "Docker Compose file path"),
    ("features", "Features to add to the dev container"),
    ("forwardPorts", "Ports to forward from the container"),
    ("customizations", "Tool-specific customizations"),
    (
        "postCreateCommand",
        "Commands to run after creating the container",
    ),
    (
        "postStartCommand",
        "Commands to run after starting the container",
    ),
    ("remoteUser", "The user to connect as"),
];

/// Generate JSONC output with section comments.
/// # Panics
///
/// Panics if the config value cannot be serialized to JSON (should not
/// happen with valid `serde_json::Value`).
pub fn to_jsonc(config: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(config).expect("config should be serializable");
    let mut lines: Vec<String> = Vec::new();

    // Prepend spec link comment
    lines.push("// For format details, see https://aka.ms/devcontainer.json.".to_owned());

    for line in pretty.lines() {
        let trimmed = line.trim_start();

        // Check if this line starts a known section
        for &(key, comment) in SECTION_COMMENTS {
            let needle = format!("\"{key}\":");
            if trimmed.starts_with(&needle) {
                let indent = &line[..line.len() - trimmed.len()];
                lines.push(String::new());
                lines.push(format!("{indent}// {comment}"));
                break;
            }
        }

        lines.push(line.to_owned());
    }

    // Append commented-out hints for common settings
    // Find the closing brace and insert before it
    if let Some(last_brace) = lines.iter().rposition(|l| l.trim() == "}") {
        let hints = vec![
            String::new(),
            "  // Uncomment to add lifecycle commands:".to_owned(),
            "  // \"postCreateCommand\": \"echo Hello!\",".to_owned(),
            String::new(),
            "  // Uncomment to connect as root:".to_owned(),
            "  // \"remoteUser\": \"root\"".to_owned(),
        ];

        // Only add hints if these keys are not already present
        let has_post_create = lines.iter().any(|l| l.contains("\"postCreateCommand\""));
        let has_remote_user = lines.iter().any(|l| l.contains("\"remoteUser\""));

        if !has_post_create && !has_remote_user {
            for (i, hint) in hints.into_iter().enumerate() {
                lines.insert(last_brace + i, hint);
            }
        }
    }

    lines.join("\n") + "\n"
}

/// Generate plain JSON output.
/// # Panics
///
/// Panics if the config value cannot be serialized to JSON.
pub fn to_json(config: &serde_json::Value) -> String {
    serde_json::to_string_pretty(config).expect("config should be serializable") + "\n"
}

/// Format a config value according to the chosen output format.
pub fn format_config(config: &serde_json::Value, format: OutputFormat) -> String {
    match format {
        OutputFormat::Jsonc => to_jsonc(config),
        OutputFormat::Json => to_json(config),
    }
}

// ---------------------------------------------------------------------------
// Template application
// ---------------------------------------------------------------------------

/// Apply a template: extract files to the output directory, substitute
/// options, merge features, and write the config.
///
/// `template_dir` is the path to the extracted template artifact.
/// `output_dir` is the workspace root (files go into `output_dir/.devcontainer/`).
///
/// # Errors
///
/// Returns [`TemplateError`] on I/O errors or invalid template structure.
pub fn apply_template<S: std::hash::BuildHasher>(
    template_id: &str,
    template_dir: &Path,
    output_dir: &Path,
    options: &HashMap<String, serde_json::Value, S>,
    features: &[SelectedFeature],
    format: OutputFormat,
    excluded_paths: &[String],
) -> Result<std::path::PathBuf, TemplateError> {
    let devcontainer_dir = output_dir.join(".devcontainer");
    std::fs::create_dir_all(&devcontainer_dir)?;

    // Find the template's .devcontainer directory
    let template_devcontainer = template_dir.join(".devcontainer");
    let source_dir = if template_devcontainer.is_dir() {
        template_devcontainer
    } else {
        // Some templates have files at the root level
        template_dir.to_path_buf()
    };

    // Compile exclude patterns
    let compiled_excludes: Vec<glob::Pattern> = excluded_paths
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    // Copy and substitute all files from the template
    copy_and_substitute(
        &source_dir,
        &devcontainer_dir,
        options,
        &source_dir,
        &compiled_excludes,
    )?;

    // Process devcontainer.json separately: substitute, parse, merge features, format.
    // Read from the source template (copy_and_substitute skips devcontainer.json).
    let config_path = devcontainer_dir.join("devcontainer.json");
    let source_config = source_dir.join("devcontainer.json");
    if source_config.exists() {
        let raw = std::fs::read_to_string(&source_config)?;
        let stripped = strip_jsonc(&raw, template_id)?;
        let substituted = substitute_template_options(&stripped, options);
        let mut config: serde_json::Value =
            serde_json::from_str(&substituted).map_err(|e| {
                let snippet: String = substituted.chars().take(80).collect();
                TemplateError::InvalidArtifact {
                    template_id: template_id.to_owned(),
                    reason: format!("invalid JSON after substitution: {e}\n  content: {snippet:?}"),
                }
            })?;

        merge_features(&mut config, features);

        let formatted = format_config(&config, format);
        std::fs::write(&config_path, formatted)?;
    }

    Ok(config_path)
}

/// Recursively copy files from `src` to `dest`, applying template option
/// substitution to text files and skipping excluded paths.
fn copy_and_substitute<S: std::hash::BuildHasher>(
    src: &Path,
    dest: &Path,
    options: &HashMap<String, serde_json::Value, S>,
    template_root: &Path,
    excluded_paths: &[glob::Pattern],
) -> Result<(), TemplateError> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name();
        let src_path = entry.path();
        let dest_path = dest.join(&file_name);

        // Check if this path should be excluded
        if !excluded_paths.is_empty() {
            let relative = src_path.strip_prefix(template_root).unwrap_or(&src_path);
            let relative_str = relative.to_string_lossy();
            if excluded_paths.iter().any(|pat| pat.matches(&relative_str)) {
                continue;
            }
        }

        // devcontainer.json is handled separately in apply_template()
        if file_name == "devcontainer.json" {
            continue;
        }

        if file_type.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
            copy_and_substitute(
                &src_path,
                &dest_path,
                options,
                template_root,
                excluded_paths,
            )?;
        } else if file_type.is_file() {
            // Try to read as text and substitute; if it fails, copy as binary
            match std::fs::read_to_string(&src_path) {
                Ok(content) => {
                    // Strip JSONC from .json files before substitution so that
                    // user-provided option values containing '//' are not eaten.
                    let is_json = src_path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
                    let stripped = if is_json {
                        strip_jsonc(&content, &file_name.to_string_lossy())?
                    } else {
                        content
                    };
                    let substituted = substitute_template_options(&stripped, options);
                    std::fs::write(&dest_path, substituted)?;
                }
                Err(_) => {
                    std::fs::copy(&src_path, &dest_path)?;
                }
            }
        }
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[expect(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // substitute_template_options
    // -----------------------------------------------------------------------

    #[test]
    fn substitute_string_option() {
        let mut opts = HashMap::new();
        opts.insert("variant".to_owned(), serde_json::json!("bookworm"));

        let input =
            r#"{"image": "mcr.microsoft.com/devcontainers/rust:1-${templateOption:variant}"}"#;
        let result = substitute_template_options(input, &opts);
        assert_eq!(
            result,
            r#"{"image": "mcr.microsoft.com/devcontainers/rust:1-bookworm"}"#
        );
    }

    #[test]
    fn substitute_boolean_option() {
        let mut opts = HashMap::new();
        opts.insert("installMaven".to_owned(), serde_json::json!(true));

        let input = "INSTALL_MAVEN=${templateOption:installMaven}";
        let result = substitute_template_options(input, &opts);
        assert_eq!(result, "INSTALL_MAVEN=true");
    }

    #[test]
    fn substitute_preserves_unmatched_placeholders() {
        let opts = HashMap::new();
        let input = "${templateOption:unknown}";
        let result = substitute_template_options(input, &opts);
        assert_eq!(result, "${templateOption:unknown}");
    }

    #[test]
    fn substitute_multiple_occurrences() {
        let mut opts = HashMap::new();
        opts.insert("ver".to_owned(), serde_json::json!("3.14"));

        let input = "a=${templateOption:ver} b=${templateOption:ver}";
        let result = substitute_template_options(input, &opts);
        assert_eq!(result, "a=3.14 b=3.14");
    }

    // -----------------------------------------------------------------------
    // merge_features
    // -----------------------------------------------------------------------

    #[test]
    fn merge_features_empty() {
        let mut config = serde_json::json!({"name": "test"});
        merge_features(&mut config, &[]);
        assert!(config.get("features").is_none());
    }

    #[test]
    fn merge_features_single() {
        let mut config = serde_json::json!({"name": "test"});
        let features = vec![SelectedFeature {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            options: HashMap::new(),
        }];
        merge_features(&mut config, &features);

        let feats = config.get("features").unwrap();
        assert!(feats.get("ghcr.io/devcontainers/features/node:1").is_some());
    }

    #[test]
    fn merge_features_with_options() {
        let mut config = serde_json::json!({"name": "test"});
        let mut opts = HashMap::new();
        opts.insert("version".to_owned(), serde_json::json!("lts"));

        let features = vec![SelectedFeature {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            options: opts,
        }];
        merge_features(&mut config, &features);

        let node = config
            .get("features")
            .unwrap()
            .get("ghcr.io/devcontainers/features/node:1")
            .unwrap();
        assert_eq!(node.get("version").unwrap(), "lts");
    }

    // -----------------------------------------------------------------------
    // JSONC generation
    // -----------------------------------------------------------------------

    #[test]
    fn jsonc_has_spec_link_comment() {
        let config = serde_json::json!({"name": "test"});
        let result = to_jsonc(&config);
        assert!(result.starts_with("// For format details"));
    }

    #[test]
    fn jsonc_has_section_comments() {
        let config = serde_json::json!({
            "name": "Rust",
            "image": "mcr.microsoft.com/devcontainers/rust:1",
            "features": {}
        });
        let result = to_jsonc(&config);
        assert!(result.contains("// A name for the dev container"));
        assert!(result.contains("// The base image to use"));
        assert!(result.contains("// Features to add to the dev container"));
    }

    #[test]
    fn jsonc_has_hints_when_no_lifecycle() {
        let config = serde_json::json!({"name": "test"});
        let result = to_jsonc(&config);
        assert!(result.contains("// Uncomment to add lifecycle commands"));
        assert!(result.contains("// Uncomment to connect as root"));
    }

    #[test]
    fn jsonc_omits_hints_when_lifecycle_present() {
        let config = serde_json::json!({
            "name": "test",
            "postCreateCommand": "echo hi",
            "remoteUser": "vscode"
        });
        let result = to_jsonc(&config);
        assert!(!result.contains("// Uncomment to add lifecycle commands"));
    }

    #[test]
    fn json_is_valid_json() {
        let config = serde_json::json!({"name": "test", "image": "ubuntu"});
        let result = to_json(&config);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["name"], "test");
    }

    // -----------------------------------------------------------------------
    // apply_template (full integration with tempdir)
    // -----------------------------------------------------------------------

    #[test]
    fn apply_template_creates_config() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        // Create template structure
        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{"name": "Test", "image": "ubuntu:${templateOption:variant}"}"#,
        )
        .unwrap();

        let mut options = HashMap::new();
        options.insert("variant".to_owned(), serde_json::json!("noble"));

        let config_path = apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &options,
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        assert!(config_path.exists());
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("ubuntu:noble"));
        assert!(!content.contains("${templateOption:variant}"));
    }

    #[test]
    fn apply_template_with_features() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{"name": "Test", "image": "ubuntu"}"#,
        )
        .unwrap();

        let features = vec![SelectedFeature {
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            options: HashMap::new(),
        }];

        let config_path = apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &HashMap::new(),
            &features,
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("features").is_some());
        assert!(
            parsed["features"]
                .get("ghcr.io/devcontainers/features/node:1")
                .is_some()
        );
    }

    #[test]
    fn apply_template_copies_extra_files() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(dc_dir.join("devcontainer.json"), r#"{"name": "Test"}"#).unwrap();
        std::fs::write(
            dc_dir.join("Dockerfile"),
            "FROM ubuntu:${templateOption:variant}\nRUN apt-get update",
        )
        .unwrap();

        let mut options = HashMap::new();
        options.insert("variant".to_owned(), serde_json::json!("noble"));

        apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &options,
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let dockerfile = output_dir.path().join(".devcontainer").join("Dockerfile");
        assert!(dockerfile.exists());
        let content = std::fs::read_to_string(&dockerfile).unwrap();
        assert!(content.contains("FROM ubuntu:noble"));
    }

    #[test]
    fn apply_template_excludes_optional_paths() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(dc_dir.join("devcontainer.json"), r#"{"name": "Test"}"#).unwrap();

        // Create a .github directory with a file (optional path)
        let github_dir = dc_dir.join(".github");
        std::fs::create_dir_all(&github_dir).unwrap();
        std::fs::write(github_dir.join("workflow.yml"), "name: CI").unwrap();

        // Create a non-optional file
        std::fs::write(dc_dir.join("Dockerfile"), "FROM ubuntu").unwrap();

        apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &HashMap::new(),
            &[],
            OutputFormat::Json,
            &[".github/*".to_owned()],
        )
        .unwrap();

        // Config and Dockerfile should exist
        assert!(
            output_dir
                .path()
                .join(".devcontainer/devcontainer.json")
                .exists()
        );
        assert!(output_dir.path().join(".devcontainer/Dockerfile").exists());

        // .github/workflow.yml should be excluded
        assert!(
            !output_dir
                .path()
                .join(".devcontainer/.github/workflow.yml")
                .exists()
        );
    }

    // -----------------------------------------------------------------------
    // JSONC template regression tests
    // -----------------------------------------------------------------------

    #[test]
    fn apply_template_jsonc_with_line_comments() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "// For format details, see https://aka.ms/devcontainer.json.\n\
             {\n  \"name\": \"Test\",\n  \"image\": \"ubuntu\"\n}\n",
        )
        .unwrap();

        let config_path = apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &HashMap::new(),
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["name"], "Test");
    }

    #[test]
    fn apply_template_jsonc_with_block_comments() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "{\n  /* block comment */\n  \"name\": \"Test\",\n  \"image\": \"ubuntu\"\n}\n",
        )
        .unwrap();

        let config_path = apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &HashMap::new(),
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["name"], "Test");
    }

    #[test]
    fn apply_template_jsonc_with_trailing_commas() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "{\n  \"name\": \"Test\",\n  \"image\": \"ubuntu\",\n}\n",
        )
        .unwrap();

        let config_path = apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &HashMap::new(),
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["name"], "Test");
    }

    #[test]
    fn apply_template_jsonc_comments_and_substitution() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "// Template: Node.js\n\
             {\n  \"name\": \"Node\",\n  \
             \"image\": \"mcr.microsoft.com/devcontainers/javascript-node:1-${templateOption:imageVariant}\"\n}\n",
        )
        .unwrap();

        let mut options = HashMap::new();
        options.insert("imageVariant".to_owned(), serde_json::json!("24-trixie"));

        let config_path = apply_template(
            "javascript-node",
            template_dir.path(),
            output_dir.path(),
            &options,
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("javascript-node:1-24-trixie"));
        assert!(!content.contains("${templateOption:"));
        assert!(!content.contains("//"));
    }

    #[test]
    fn apply_template_option_with_slashes_not_stripped() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "// comment\n{\"url\": \"${templateOption:url}\"}",
        )
        .unwrap();

        let mut options = HashMap::new();
        options.insert("url".to_owned(), serde_json::json!("http://example.com"));

        let config_path = apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &options,
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("http://example.com"));
    }

    #[test]
    fn apply_template_non_devcontainer_json_stripped() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(dc_dir.join("devcontainer.json"), r#"{"name": "Test"}"#).unwrap();

        // Create a .json file with JSONC comments
        std::fs::write(
            dc_dir.join("settings.json"),
            "// VS Code settings\n{\"editor.fontSize\": 14, }",
        )
        .unwrap();

        apply_template(
            "test",
            template_dir.path(),
            output_dir.path(),
            &HashMap::new(),
            &[],
            OutputFormat::Json,
            &[],
        )
        .unwrap();

        let settings = std::fs::read_to_string(
            output_dir.path().join(".devcontainer/settings.json"),
        )
        .unwrap();
        // Comments should be stripped, trailing comma removed
        assert!(!settings.contains("//"));
        // Should still contain the actual data
        assert!(settings.contains("\"editor.fontSize\": 14"));
    }

    #[test]
    fn copy_substitution_skips_devcontainer_json() {
        let template_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{"name": "${templateOption:name}"}"#,
        )
        .unwrap();
        std::fs::write(
            dc_dir.join("Dockerfile"),
            "FROM ${templateOption:image}",
        )
        .unwrap();

        let dest_dc = output_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dest_dc).unwrap();

        let mut options = HashMap::new();
        options.insert("name".to_owned(), serde_json::json!("Test"));
        options.insert("image".to_owned(), serde_json::json!("ubuntu"));

        copy_and_substitute(&dc_dir, &dest_dc, &options, &dc_dir, &[]).unwrap();

        // Dockerfile should be copied and substituted
        let dockerfile = std::fs::read_to_string(dest_dc.join("Dockerfile")).unwrap();
        assert_eq!(dockerfile, "FROM ubuntu");

        // devcontainer.json should NOT be copied by copy_and_substitute
        assert!(!dest_dc.join("devcontainer.json").exists());
    }
}
