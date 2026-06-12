//! Template application: extract files, substitute placeholders, generate config.
//!
//! Handles the full pipeline from an extracted template artifact to a written
//! `.devcontainer/` directory with JSONC or JSON output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// Result of substituting `${templateOption:KEY}` placeholders into content.
#[derive(Debug, Default)]
pub struct SubstitutionReport {
    /// Substituted content with all known tokens replaced (unknown tokens
    /// become `""`).
    pub content: String,
    /// Tokens that appeared in the content but had no matching option key.
    /// Callers should emit a warning per entry so users can diagnose typos.
    pub unknown_tokens: Vec<String>,
}

/// Substitute `${templateOption:KEY}` placeholders in content.
///
/// - **Known tokens**: replaced with the option value (stringified). Falsy
///   values (`"false"`, `"0"`, `""`) are preserved as-is — only absent keys
///   fall back to `""`.
/// - **Unknown tokens**: substituted with `""` and recorded in
///   [`SubstitutionReport::unknown_tokens`] so callers can warn the user.
/// - **Whitespace tolerance**: `${templateOption: key }` (spaces around the
///   key name) is matched and treated identically to `${templateOption:key}`.
///
/// This differs from the upstream `apply.ts` implementation which uses
/// `options[token] || ''`, collapsing falsy values to empty string (a bug).
pub fn substitute_template_options<S: std::hash::BuildHasher>(
    content: &str,
    options: &HashMap<String, serde_json::Value, S>,
) -> SubstitutionReport {
    // Regex: ${templateOption: key } — key is captured, whitespace trimmed.
    static TOKEN_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$\{templateOption:\s*(\w+?)\s*\}").expect("valid regex")
    });

    let mut unknown_tokens: Vec<String> = Vec::new();
    let content_out = TOKEN_RE.replace_all(content, |caps: &regex::Captures<'_>| {
        let key = caps.get(1).map_or("", |m| m.as_str());
        options.get(key).map_or_else(
            || {
                unknown_tokens.push(key.to_owned());
                String::new()
            },
            |value| match value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            },
        )
    });

    SubstitutionReport {
        content: content_out.into_owned(),
        unknown_tokens,
    }
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
// Config overrides
// ---------------------------------------------------------------------------

/// User customizations applied on top of the template during generation.
#[derive(Debug, Default)]
pub struct ConfigOverrides {
    /// Custom name for the dev container (overrides template's name).
    pub name: Option<String>,
    /// Full pinned image reference (replaces entire `"image"` field value).
    pub pinned_image: Option<String>,
    /// Template paths to exclude from the output.
    pub excluded_paths: Vec<String>,
}

/// Apply user-specified overrides to the parsed config.
pub fn apply_overrides(config: &mut serde_json::Value, overrides: &ConfigOverrides) {
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    if let Some(name) = &overrides.name {
        obj.insert("name".to_owned(), serde_json::Value::String(name.clone()));
    }
    if let Some(image) = &overrides.pinned_image {
        obj.insert("image".to_owned(), serde_json::Value::String(image.clone()));
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
// Template application (init path: copies into .devcontainer/)
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
    overrides: &ConfigOverrides,
) -> Result<PathBuf, TemplateError> {
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
    let compiled_excludes: Vec<glob::Pattern> = overrides
        .excluded_paths
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
        let report = substitute_template_options(&stripped, options);
        // Unknown tokens in devcontainer.json: warn but continue (output compat).
        for token in &report.unknown_tokens {
            tracing::warn!(
                "template {template_id}: unknown templateOption token '{token}' in devcontainer.json — substituting empty string"
            );
        }
        let substituted = report.content;
        let mut config: serde_json::Value = serde_json::from_str(&substituted).map_err(|e| {
            let snippet: String = substituted.chars().take(80).collect();
            TemplateError::InvalidArtifact {
                template_id: template_id.to_owned(),
                reason: format!("invalid JSON after substitution: {e}\n  content: {snippet:?}"),
            }
        })?;

        apply_overrides(&mut config, overrides);
        merge_features(&mut config, features);

        let formatted = format_config(&config, format);
        std::fs::write(&config_path, formatted)?;
    }

    Ok(config_path)
}

// ---------------------------------------------------------------------------
// apply_to_workspace (official `templates apply` path: extracts to workspace root)
// ---------------------------------------------------------------------------

/// Always-excluded filenames in `templates apply` extraction.
const ALWAYS_EXCLUDED: &[&str] = &["devcontainer-template.json", "README.md", "NOTES.md"];

/// Apply a template to a workspace root directory, following the official
/// `devcontainer templates apply` contract.
///
/// Unlike [`apply_template`] (which writes into `.devcontainer/`), this
/// function extracts the full tarball contents to `workspace_folder`, mirroring
/// the official CLI's extraction semantics.
///
/// Files excluded:
/// - `devcontainer-template.json`, `README.md`, `NOTES.md` (always)
/// - Any path matched by an entry in `omit_paths` (supports trailing `/*` globs)
///
/// Template option placeholders (`${templateOption:KEY}`) are substituted in
/// text files; binary files (not valid UTF-8) are copied verbatim.
///
/// Returns a list of written paths relative to `workspace_folder`.
///
/// # Errors
///
/// Returns [`TemplateError`] on I/O errors or invalid template structure.
pub fn apply_to_workspace<S: std::hash::BuildHasher>(
    template_id: &str,
    template_dir: &Path,
    workspace_folder: &Path,
    options: &HashMap<String, serde_json::Value, S>,
    omit_paths: &[String],
) -> Result<Vec<PathBuf>, TemplateError> {
    std::fs::create_dir_all(workspace_folder)?;

    // Compile omit-path patterns.
    let omit_patterns: Vec<glob::Pattern> = omit_paths
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    let mut written_files: Vec<PathBuf> = Vec::new();
    extract_to_workspace(
        template_dir,
        workspace_folder,
        template_id,
        options,
        template_dir,
        &omit_patterns,
        &mut written_files,
    )?;

    written_files.sort();
    Ok(written_files)
}

/// Recursively walk `src`, apply substitution, write to `dest`, and collect
/// relative paths of written files.
fn extract_to_workspace<S: std::hash::BuildHasher>(
    src: &Path,
    dest: &Path,
    template_id: &str,
    options: &HashMap<String, serde_json::Value, S>,
    template_root: &Path,
    omit_patterns: &[glob::Pattern],
    written_files: &mut Vec<PathBuf>,
) -> Result<(), TemplateError> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name();
        let src_path = entry.path();
        let dest_path = dest.join(&file_name);

        // Compute path relative to template root for exclusion matching.
        let relative = src_path.strip_prefix(template_root).unwrap_or(&src_path);
        let relative_str = relative.to_string_lossy();

        // Always-excluded filenames (only at root level by name).
        if ALWAYS_EXCLUDED
            .iter()
            .any(|ex| file_name.to_string_lossy() == *ex)
        {
            continue;
        }

        // Omit-path glob matching — check the entry itself.
        if omit_patterns.iter().any(|pat| pat.matches(&relative_str)) {
            continue;
        }

        if file_type.is_dir() {
            // Also skip a directory if any omit pattern covers its children
            // (e.g. `.github/*` → skip `.github` dir entirely).
            let dir_prefix = format!("{relative_str}/");
            if omit_patterns
                .iter()
                .any(|pat| pat.as_str().starts_with(&dir_prefix))
            {
                continue;
            }

            std::fs::create_dir_all(&dest_path)?;
            extract_to_workspace(
                &src_path,
                &dest_path,
                template_id,
                options,
                template_root,
                omit_patterns,
                written_files,
            )?;
        } else if file_type.is_file() {
            write_substituted_file(template_id, &src_path, &dest_path, options)?;
            // Record path relative to workspace folder (= relative to template root,
            // since both share the same directory layout).
            written_files.push(relative.to_path_buf());
        }
    }
    Ok(())
}

/// Write a single file to `dest`, substituting template options.
///
/// Text files: substituted and written as UTF-8.
/// Binary files (invalid UTF-8): copied verbatim.
fn write_substituted_file<S: std::hash::BuildHasher>(
    template_id: &str,
    src: &Path,
    dest: &Path,
    options: &HashMap<String, serde_json::Value, S>,
) -> Result<(), TemplateError> {
    match std::fs::read_to_string(src) {
        Ok(content) => {
            let report = substitute_template_options(&content, options);
            for token in &report.unknown_tokens {
                tracing::warn!(
                    "template {template_id}: unknown templateOption token '{token}' in {} — substituting empty string",
                    src.display()
                );
            }
            std::fs::write(dest, report.content)?;
        }
        Err(_) => {
            // Not valid UTF-8 — copy binary verbatim.
            std::fs::copy(src, dest)?;
        }
    }
    Ok(())
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
                    let report = substitute_template_options(&stripped, options);
                    std::fs::write(&dest_path, report.content)?;
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
        let report = substitute_template_options(input, &opts);
        assert_eq!(
            report.content,
            r#"{"image": "mcr.microsoft.com/devcontainers/rust:1-bookworm"}"#
        );
        assert!(report.unknown_tokens.is_empty());
    }

    #[test]
    fn substitute_boolean_option() {
        let mut opts = HashMap::new();
        opts.insert("installMaven".to_owned(), serde_json::json!(true));

        let input = "INSTALL_MAVEN=${templateOption:installMaven}";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "INSTALL_MAVEN=true");
        assert!(report.unknown_tokens.is_empty());
    }

    #[test]
    fn substitute_falsy_false_preserved() {
        // Upstream bug: `options[token] || ''` collapses "false" to "". cella must preserve it.
        let mut opts = HashMap::new();
        opts.insert("enabled".to_owned(), serde_json::json!("false"));

        let input = "ENABLED=${templateOption:enabled}";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "ENABLED=false");
        assert!(report.unknown_tokens.is_empty());
    }

    #[test]
    fn substitute_falsy_zero_preserved() {
        let mut opts = HashMap::new();
        opts.insert("count".to_owned(), serde_json::json!("0"));

        let input = "COUNT=${templateOption:count}";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "COUNT=0");
        assert!(report.unknown_tokens.is_empty());
    }

    #[test]
    fn substitute_falsy_empty_string_preserved() {
        let mut opts = HashMap::new();
        opts.insert("prefix".to_owned(), serde_json::json!(""));

        let input = "PREFIX=${templateOption:prefix}END";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "PREFIX=END");
        assert!(report.unknown_tokens.is_empty());
    }

    #[test]
    fn substitute_unknown_token_reports_and_empties() {
        let opts: HashMap<String, serde_json::Value> = HashMap::new();
        let input = "x=${templateOption:missing}";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "x=");
        assert_eq!(report.unknown_tokens, vec!["missing"]);
    }

    #[test]
    fn substitute_whitespace_in_token() {
        let mut opts = HashMap::new();
        opts.insert("key".to_owned(), serde_json::json!("val"));

        let input = "${templateOption: key }";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "val");
        assert!(report.unknown_tokens.is_empty());
    }

    #[test]
    fn substitute_multiple_occurrences() {
        let mut opts = HashMap::new();
        opts.insert("ver".to_owned(), serde_json::json!("3.14"));

        let input = "a=${templateOption:ver} b=${templateOption:ver}";
        let report = substitute_template_options(input, &opts);
        assert_eq!(report.content, "a=3.14 b=3.14");
        assert!(report.unknown_tokens.is_empty());
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides {
                excluded_paths: vec![".github/*".to_owned()],
                ..Default::default()
            },
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
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
            &ConfigOverrides::default(),
        )
        .unwrap();

        let settings =
            std::fs::read_to_string(output_dir.path().join(".devcontainer/settings.json")).unwrap();
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
        std::fs::write(dc_dir.join("Dockerfile"), "FROM ${templateOption:image}").unwrap();

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

    // -----------------------------------------------------------------------
    // apply_overrides
    // -----------------------------------------------------------------------

    #[test]
    fn apply_overrides_sets_name() {
        let mut config = serde_json::json!({"name": "Template Name", "image": "ubuntu"});
        let overrides = ConfigOverrides {
            name: Some("My Project".to_owned()),
            pinned_image: None,
            ..Default::default()
        };
        apply_overrides(&mut config, &overrides);
        assert_eq!(config["name"], "My Project");
    }

    #[test]
    fn apply_overrides_sets_pinned_image() {
        let mut config = serde_json::json!({"name": "Test", "image": "mcr.microsoft.com/devcontainers/rust:1-trixie"});
        let overrides = ConfigOverrides {
            name: None,
            pinned_image: Some("mcr.microsoft.com/devcontainers/rust:1.87-trixie".to_owned()),
            ..Default::default()
        };
        apply_overrides(&mut config, &overrides);
        assert_eq!(
            config["image"],
            "mcr.microsoft.com/devcontainers/rust:1.87-trixie"
        );
    }

    #[test]
    fn apply_overrides_both() {
        let mut config = serde_json::json!({"name": "Old", "image": "old:tag"});
        let overrides = ConfigOverrides {
            name: Some("New".to_owned()),
            pinned_image: Some("new:pinned".to_owned()),
            ..Default::default()
        };
        apply_overrides(&mut config, &overrides);
        assert_eq!(config["name"], "New");
        assert_eq!(config["image"], "new:pinned");
    }

    #[test]
    fn apply_overrides_noop_when_empty() {
        let mut config = serde_json::json!({"name": "Test", "image": "ubuntu"});
        let original = config.clone();
        apply_overrides(&mut config, &ConfigOverrides::default());
        assert_eq!(config, original);
    }

    #[test]
    fn apply_overrides_inserts_name_when_missing() {
        let mut config = serde_json::json!({"image": "ubuntu"});
        let overrides = ConfigOverrides {
            name: Some("Added Name".to_owned()),
            pinned_image: None,
            ..Default::default()
        };
        apply_overrides(&mut config, &overrides);
        assert_eq!(config["name"], "Added Name");
    }

    // -----------------------------------------------------------------------
    // apply_to_workspace
    // -----------------------------------------------------------------------

    #[test]
    fn apply_to_workspace_basic_extraction() {
        let template_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        // Template with .devcontainer/devcontainer.json and README.md
        let dc_dir = template_dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{"name":"Test","image":"ubuntu"}"#,
        )
        .unwrap();
        std::fs::write(template_dir.path().join("README.md"), "# readme").unwrap();
        std::fs::write(
            template_dir.path().join("devcontainer-template.json"),
            r#"{"id":"test","version":"1.0"}"#,
        )
        .unwrap();

        let files = apply_to_workspace(
            "test",
            template_dir.path(),
            workspace.path(),
            &HashMap::new(),
            &[],
        )
        .unwrap();

        // README.md and devcontainer-template.json must NOT be extracted
        assert!(!workspace.path().join("README.md").exists());
        assert!(!workspace.path().join("devcontainer-template.json").exists());

        // devcontainer.json should exist
        assert!(
            workspace
                .path()
                .join(".devcontainer/devcontainer.json")
                .exists()
        );

        // The returned list should contain the relative path
        assert!(
            files
                .iter()
                .any(|p| p == Path::new(".devcontainer/devcontainer.json"))
        );
    }

    #[test]
    fn apply_to_workspace_always_excluded_notes() {
        let template_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        std::fs::write(template_dir.path().join("NOTES.md"), "notes").unwrap();
        std::fs::write(template_dir.path().join("README.md"), "readme").unwrap();
        std::fs::write(
            template_dir.path().join("devcontainer-template.json"),
            r#"{"id":"t","version":"1"}"#,
        )
        .unwrap();
        // A real file that should be extracted
        std::fs::write(template_dir.path().join("setup.sh"), "#!/bin/sh").unwrap();

        let files = apply_to_workspace(
            "test",
            template_dir.path(),
            workspace.path(),
            &HashMap::new(),
            &[],
        )
        .unwrap();

        assert!(!workspace.path().join("NOTES.md").exists());
        assert!(!workspace.path().join("README.md").exists());
        assert!(!workspace.path().join("devcontainer-template.json").exists());
        assert!(workspace.path().join("setup.sh").exists());
        assert_eq!(files, vec![PathBuf::from("setup.sh")]);
    }

    #[test]
    fn apply_to_workspace_omit_paths_glob() {
        let template_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let github_dir = template_dir.path().join(".github");
        let workflows_dir = github_dir.join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::write(workflows_dir.join("ci.yaml"), "name: CI").unwrap();
        std::fs::write(template_dir.path().join("setup.sh"), "#!/bin/sh").unwrap();

        let files = apply_to_workspace(
            "test",
            template_dir.path(),
            workspace.path(),
            &HashMap::new(),
            &[".github/*".to_owned()],
        )
        .unwrap();

        // .github/* should exclude the directory entry
        assert!(!workspace.path().join(".github").exists());
        assert!(workspace.path().join("setup.sh").exists());
        assert_eq!(files, vec![PathBuf::from("setup.sh")]);
    }

    #[test]
    fn apply_to_workspace_substitution_falsy() {
        let template_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        std::fs::write(
            template_dir.path().join("config.txt"),
            "FLAG=${templateOption:flag}",
        )
        .unwrap();

        let mut opts = HashMap::new();
        opts.insert("flag".to_owned(), serde_json::json!("false"));

        apply_to_workspace("test", template_dir.path(), workspace.path(), &opts, &[]).unwrap();

        let content = std::fs::read_to_string(workspace.path().join("config.txt")).unwrap();
        assert_eq!(content, "FLAG=false");
    }

    #[test]
    fn apply_to_workspace_binary_copied_verbatim() {
        let template_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        // Write raw bytes that are not valid UTF-8
        let binary_data: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x80, 0x90];
        std::fs::write(template_dir.path().join("data.bin"), &binary_data).unwrap();

        apply_to_workspace(
            "test",
            template_dir.path(),
            workspace.path(),
            &HashMap::new(),
            &[],
        )
        .unwrap();

        let result = std::fs::read(workspace.path().join("data.bin")).unwrap();
        assert_eq!(result, binary_data);
    }
}
