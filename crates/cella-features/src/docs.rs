//! `cella features generate-docs` — render `README.md` per feature in a collection.
//!
//! Matches the official devcontainer CLI output format verbatim: same section
//! headers, usage JSON block, options table columns, notes file inclusion, and
//! footer note.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tracing::warn;

/// Input parameters for the docs generator.
pub struct GenerateDocsInput<'a> {
    /// Path to the folder containing feature subdirectories.
    /// Each subdirectory that holds a `devcontainer-feature.json` gets a `README.md`.
    pub project_folder: &'a Path,
    /// OCI registry name (e.g. `ghcr.io`).
    pub registry: &'a str,
    /// Collection namespace (e.g. `owner/repo`).
    pub namespace: &'a str,
    /// GitHub owner — used to build the `devcontainer-feature.json` link in the footer.
    pub github_owner: &'a str,
    /// GitHub repo — used to build the `devcontainer-feature.json` link in the footer.
    pub github_repo: &'a str,
}

/// Per-feature generation result.
#[derive(Debug)]
pub struct FeatureDocsResult {
    /// Path to the written `README.md`.
    pub readme_path: PathBuf,
    /// Feature id.
    pub feature_id: String,
}

/// Outcome of a single feature directory scan.
#[derive(Debug)]
enum ScanOutcome {
    /// Skipped — no `devcontainer-feature.json` or missing `id`.
    Skipped,
    /// README was generated and written.
    Generated(FeatureDocsResult),
}

/// Generate `README.md` for every feature found under `input.project_folder`.
///
/// Directories beginning with `.` are skipped. Directories without a
/// `devcontainer-feature.json` emit a warning and are skipped. Existing
/// `README.md` files are overwritten.
///
/// # Errors
///
/// Returns an error only when the project folder itself cannot be read.
/// Per-feature parse/write errors are logged as warnings and skipped, matching
/// the official CLI's exit-0 behaviour.
pub fn generate_docs(
    input: &GenerateDocsInput<'_>,
) -> Result<Vec<FeatureDocsResult>, std::io::Error> {
    let entries = std::fs::read_dir(input.project_folder)?;

    let mut results = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to read directory entry: {e}");
                continue;
            }
        };
        let name = entry.file_name();
        let dir_name = name.to_string_lossy();

        // Skip dotfiles / hidden directories.
        if dir_name.starts_with('.') {
            continue;
        }

        let is_dir = match entry.file_type() {
            Ok(ft) => ft.is_dir(),
            Err(e) => {
                warn!("Failed to stat '{}': {e}", entry.path().display());
                continue;
            }
        };
        if !is_dir {
            continue;
        }

        let feature_dir = entry.path();
        match process_feature_dir(&feature_dir, &dir_name, input) {
            ScanOutcome::Skipped => {}
            ScanOutcome::Generated(r) => results.push(r),
        }
    }

    Ok(results)
}

/// Process one feature subdirectory, returning the scan outcome.
fn process_feature_dir(
    feature_dir: &Path,
    dir_name: &str,
    input: &GenerateDocsInput<'_>,
) -> ScanOutcome {
    let manifest_path = feature_dir.join("devcontainer-feature.json");

    if !manifest_path.exists() {
        warn!(
            "(!) Warning: devcontainer-feature.json not found at path '{}'. Skipping...",
            manifest_path.display()
        );
        return ScanOutcome::Skipped;
    }

    let raw_text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to read {}: {e}", manifest_path.display());
            return ScanOutcome::Skipped;
        }
    };

    // Strip JSONC (comments + trailing commas) before parsing, matching the
    // official CLI which uses jsonc-parser and accepts JSONC manifests.
    let json_text = match cella_jsonc::strip(&raw_text) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                "Failed to strip JSONC from {}: {e}",
                manifest_path.display()
            );
            return ScanOutcome::Skipped;
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&json_text) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse {}: {e}", manifest_path.display());
            return ScanOutcome::Skipped;
        }
    };

    let Some(feature_id) = parsed.get("id").and_then(|v| v.as_str()).map(str::to_owned) else {
        warn!("devcontainer-feature.json for '{dir_name}' does not contain an 'id'");
        return ScanOutcome::Skipped;
    };

    let readme_content = render_readme(&parsed, &feature_id, feature_dir, input);

    let readme_path = feature_dir.join("README.md");
    // Remove existing README before writing (matches official CLI behaviour).
    let _ = std::fs::remove_file(&readme_path);

    if let Err(e) = std::fs::write(&readme_path, &readme_content) {
        warn!("Failed to write {}: {e}", readme_path.display());
        return ScanOutcome::Skipped;
    }

    ScanOutcome::Generated(FeatureDocsResult {
        readme_path,
        feature_id,
    })
}

/// Render the full README content for one feature.
fn render_readme(
    parsed: &serde_json::Value,
    feature_id: &str,
    feature_dir: &Path,
    input: &GenerateDocsInput<'_>,
) -> String {
    let name = build_name(parsed, feature_id);
    let description = parsed
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    let version = extract_major_version(parsed);
    let repo_url = build_repo_url(feature_id, input);

    let options_table = generate_options_markdown(parsed);
    let customizations = generate_customizations_markdown(parsed);
    let notes = read_notes(feature_dir);
    let deprecation = generate_deprecation_header(parsed);

    format!(
        "{deprecation}# {name}\n\n{description}\n\n## Example Usage\n\n```json\n\"features\": {{\n    \"{registry}/{namespace}/{feature_id}:{version}\": {{}}\n}}\n```\n\n{options_table}{customizations}{notes}\n---\n\n_Note: This file was auto-generated from the [devcontainer-feature.json]({repo_url}).  Add additional notes to a `NOTES.md`._\n",
        registry = input.registry,
        namespace = input.namespace,
    )
}

/// Build the display name: `"Name (id)"` when a `name` field is present, else just `id`.
fn build_name(parsed: &serde_json::Value, feature_id: &str) -> String {
    parsed
        .get("name")
        .and_then(|v| v.as_str())
        .map_or_else(|| feature_id.to_owned(), |n| format!("{n} ({feature_id})"))
}

/// Extract the major version number from the `version` field (e.g. `"1.2.3"` → `"1"`).
/// Falls back to `"latest"` when the field is absent or unparseable.
fn extract_major_version(parsed: &serde_json::Value) -> String {
    parsed
        .get("version")
        .and_then(|v| v.as_str())
        .and_then(|v| v.split('.').next())
        .filter(|s| !s.is_empty())
        .map_or_else(|| "latest".to_owned(), str::to_owned)
}

/// Build the URL (or bare filename) for the footer link to `devcontainer-feature.json`.
///
/// Normalizes the project folder path: strips leading `./`, collapses a bare
/// `.` (current-directory default) to an empty segment, and removes trailing
/// slashes.  The resulting URL path is either `…/blob/main/feature/…` (when
/// the project folder is `.` / `./`) or `…/blob/main/src/feature/…` (when a
/// real sub-path is given).
fn build_repo_url(feature_id: &str, input: &GenerateDocsInput<'_>) -> String {
    if input.github_owner.is_empty() || input.github_repo.is_empty() {
        return "devcontainer-feature.json".to_owned();
    }

    // Normalize: strip leading `./`, then strip a lone `.`, then strip trailing
    // slashes.  This makes the default `--project-folder .` produce a clean URL
    // with no stray `./` segment (matching the official CLI behaviour).
    let raw = input.project_folder.to_string_lossy();
    let trimmed = raw.strip_prefix("./").unwrap_or(&raw).trim_end_matches('/');
    // A bare `.` means "current directory" — collapse to empty so we don't
    // embed a literal `.` in the URL.
    let base = if trimmed == "." { "" } else { trimmed };

    if base.is_empty() {
        format!(
            "https://github.com/{owner}/{repo}/blob/main/{feature_id}/devcontainer-feature.json",
            owner = input.github_owner,
            repo = input.github_repo,
        )
    } else {
        format!(
            "https://github.com/{owner}/{repo}/blob/main/{base}/{feature_id}/devcontainer-feature.json",
            owner = input.github_owner,
            repo = input.github_repo,
        )
    }
}

/// Generate the options table markdown, or an empty string when there are no options.
fn generate_options_markdown(parsed: &serde_json::Value) -> String {
    let options = match parsed.get("options").and_then(|v| v.as_object()) {
        Some(o) if !o.is_empty() => o,
        _ => return String::new(),
    };

    let mut rows = String::new();
    for (key, val) in options {
        let desc = val
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let typ = val.get("type").and_then(|v| v.as_str()).unwrap_or("-");
        let default = format_default(val.get("default"));
        let _ = writeln!(rows, "| {key} | {desc} | {typ} | {default} |");
    }

    format!(
        "## Options\n\n| Options Id | Description | Type | Default Value |\n|-----|-----|-----|-----|\n{rows}\n"
    )
}

/// Format a `default` JSON value for the options table.
///
/// Matches official CLI: empty string becomes `"-"`, `None` becomes `"-"`.
fn format_default(default: Option<&serde_json::Value>) -> String {
    match default {
        None => "-".to_owned(),
        Some(serde_json::Value::String(s)) if s.is_empty() => "-".to_owned(),
        Some(v) => match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            other => other.to_string(),
        },
    }
}

/// Generate the customizations block (VS Code extensions), or empty string.
fn generate_customizations_markdown(parsed: &serde_json::Value) -> String {
    let extensions = parsed
        .pointer("/customizations/vscode/extensions")
        .and_then(|v| v.as_array());

    let Some(exts) = extensions else {
        return String::new();
    };

    if exts.is_empty() {
        return String::new();
    }

    let mut lines = String::from("\n## Customizations\n\n### VS Code Extensions\n\n");
    for ext in exts {
        if let Some(s) = ext.as_str() {
            let _ = writeln!(lines, "- `{s}`");
        }
    }
    lines
}

/// Read `NOTES.md` contents from the feature directory, or return empty string.
fn read_notes(feature_dir: &Path) -> String {
    let notes_path = feature_dir.join("NOTES.md");
    if notes_path.exists() {
        std::fs::read_to_string(&notes_path).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Generate the deprecation / legacy-ids header block when applicable.
fn generate_deprecation_header(parsed: &serde_json::Value) -> String {
    let deprecated = parsed
        .get("deprecated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let legacy_ids: Vec<&str> = parsed
        .get("legacyIds")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if !deprecated && legacy_ids.is_empty() {
        return String::new();
    }

    let mut header = String::from("### **IMPORTANT NOTE**\n\n");
    if deprecated {
        header.push_str(
            "- **This Feature is deprecated, and will no longer receive any further updates/support.**\n",
        );
    }
    if !legacy_ids.is_empty() {
        let ids = legacy_ids
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            header,
            "- **Ids used to publish this Feature in the past - {ids}**"
        );
    }
    // Trailing newline separates the deprecation block from the title that follows.
    header.push('\n');
    header
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn make_feature_dir(tmp: &TempDir, id: &str, manifest: &serde_json::Value) -> PathBuf {
        let dir = tmp.path().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("devcontainer-feature.json"),
            serde_json::to_string_pretty(manifest).unwrap(),
        )
        .unwrap();
        dir
    }

    fn default_input<'a>(tmp: &'a TempDir, namespace: &'a str) -> GenerateDocsInput<'a> {
        GenerateDocsInput {
            project_folder: tmp.path(),
            registry: "ghcr.io",
            namespace,
            github_owner: "",
            github_repo: "",
        }
    }

    // -------------------------------------------------------------------------
    // First-line assertions — no leading blank line
    // -------------------------------------------------------------------------

    #[test]
    fn readme_starts_with_title_no_leading_blank() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "plain", "version": "1.0.0", "name": "Plain" });
        make_feature_dir(&tmp, "plain", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        let first_line = readme.lines().next().unwrap_or("");
        assert_eq!(
            first_line, "# Plain (plain)",
            "first line must be the title, got:\n{readme}"
        );
    }

    #[test]
    fn deprecated_readme_starts_with_important_note() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "old", "version": "1.0.0", "deprecated": true });
        make_feature_dir(&tmp, "old", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        let first_line = readme.lines().next().unwrap_or("");
        assert_eq!(
            first_line, "### **IMPORTANT NOTE**",
            "deprecated feature first line must be the deprecation header, got:\n{readme}"
        );
    }

    // -------------------------------------------------------------------------
    // Title / name
    // -------------------------------------------------------------------------

    #[test]
    fn title_with_name_field() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({
            "id": "my-feat",
            "name": "My Feature",
            "version": "1.2.3",
            "description": "Does things."
        });
        make_feature_dir(&tmp, "my-feat", &manifest);
        let input = default_input(&tmp, "owner/repo");

        let results = generate_docs(&input).unwrap();
        assert_eq!(results.len(), 1);
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(readme.contains("# My Feature (my-feat)"), "got:\n{readme}");
    }

    #[test]
    fn title_without_name_field() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "bare-feat", "version": "2.0.0" });
        make_feature_dir(&tmp, "bare-feat", &manifest);
        let input = default_input(&tmp, "owner/repo");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(readme.contains("# bare-feat\n"), "got:\n{readme}");
    }

    // -------------------------------------------------------------------------
    // Usage block
    // -------------------------------------------------------------------------

    #[test]
    fn usage_block_major_version() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "node", "version": "3.7.1" });
        make_feature_dir(&tmp, "node", &manifest);
        let input = default_input(&tmp, "myorg/features");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(
            readme.contains("\"ghcr.io/myorg/features/node:3\": {}"),
            "got:\n{readme}"
        );
    }

    #[test]
    fn usage_block_no_version_falls_back_to_latest() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "myfeat" });
        make_feature_dir(&tmp, "myfeat", &manifest);
        let input = default_input(&tmp, "myorg/features");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(
            readme.contains("\"ghcr.io/myorg/features/myfeat:latest\": {}"),
            "got:\n{readme}"
        );
    }

    // -------------------------------------------------------------------------
    // Options table
    // -------------------------------------------------------------------------

    #[test]
    fn options_table_columns_and_rows() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({
            "id": "feat",
            "version": "1.0.0",
            "options": {
                "version": {
                    "type": "string",
                    "default": "lts",
                    "description": "Node version"
                },
                "debug": {
                    "type": "boolean",
                    "default": false,
                    "description": "Enable debug"
                },
                "mode": {
                    "type": "string",
                    "default": "",
                    "description": "Run mode",
                    "enum": ["a", "b"]
                }
            }
        });
        make_feature_dir(&tmp, "feat", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();

        // Header row
        assert!(
            readme.contains("| Options Id | Description | Type | Default Value |"),
            "got:\n{readme}"
        );
        // Separator
        assert!(
            readme.contains("|-----|-----|-----|-----|"),
            "got:\n{readme}"
        );
        // Rows
        assert!(
            readme.contains("| version | Node version | string | lts |"),
            "got:\n{readme}"
        );
        assert!(
            readme.contains("| debug | Enable debug | boolean | false |"),
            "got:\n{readme}"
        );
        // Empty default becomes "-"
        assert!(
            readme.contains("| mode | Run mode | string | - |"),
            "got:\n{readme}"
        );
    }

    #[test]
    fn no_options_section_when_empty() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "feat", "version": "1.0.0" });
        make_feature_dir(&tmp, "feat", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(!readme.contains("## Options"), "got:\n{readme}");
    }

    // -------------------------------------------------------------------------
    // NOTES.md inclusion
    // -------------------------------------------------------------------------

    #[test]
    fn notes_md_included_when_present() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "feat", "version": "1.0.0" });
        let dir = make_feature_dir(&tmp, "feat", &manifest);
        std::fs::write(dir.join("NOTES.md"), "Extra notes here.\n").unwrap();
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(readme.contains("Extra notes here."), "got:\n{readme}");
    }

    #[test]
    fn notes_md_absent_is_noop() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "feat", "version": "1.0.0" });
        make_feature_dir(&tmp, "feat", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        // Footer should still be present; no NOTES content.
        assert!(
            readme.contains("auto-generated from the [devcontainer-feature.json]"),
            "got:\n{readme}"
        );
    }

    // -------------------------------------------------------------------------
    // Footer note
    // -------------------------------------------------------------------------

    #[test]
    fn footer_bare_filename_without_github_args() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "feat", "version": "1.0.0" });
        make_feature_dir(&tmp, "feat", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(
            readme.contains("[devcontainer-feature.json](devcontainer-feature.json)"),
            "got:\n{readme}"
        );
    }

    #[test]
    fn footer_full_url_with_github_args() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "feat", "version": "1.0.0" });
        make_feature_dir(&tmp, "feat", &manifest);
        let input = GenerateDocsInput {
            project_folder: tmp.path(),
            registry: "ghcr.io",
            namespace: "o/r",
            github_owner: "myowner",
            github_repo: "myrepo",
        };

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(
            readme.contains("https://github.com/myowner/myrepo/blob/main/"),
            "got:\n{readme}"
        );
        assert!(
            readme.contains("/feat/devcontainer-feature.json"),
            "got:\n{readme}"
        );
    }

    // -------------------------------------------------------------------------
    // Missing manifest → skip with warning
    // -------------------------------------------------------------------------

    #[test]
    fn missing_manifest_skipped_no_readme_written() {
        let tmp = TempDir::new().unwrap();
        // Create a dir without a manifest.
        let dir = tmp.path().join("no-manifest");
        std::fs::create_dir_all(&dir).unwrap();
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        // No README generated.
        assert!(results.is_empty());
        assert!(!dir.join("README.md").exists());
    }

    // -------------------------------------------------------------------------
    // Existing README overwritten
    // -------------------------------------------------------------------------

    #[test]
    fn existing_readme_is_overwritten() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({ "id": "feat", "version": "1.0.0" });
        let dir = make_feature_dir(&tmp, "feat", &manifest);
        std::fs::write(dir.join("README.md"), "old content").unwrap();
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(!readme.contains("old content"), "got:\n{readme}");
        assert!(
            readme.contains("auto-generated from the [devcontainer-feature.json]"),
            "got:\n{readme}"
        );
    }

    // -------------------------------------------------------------------------
    // Dotfile dirs skipped
    // -------------------------------------------------------------------------

    #[test]
    fn dotfile_dirs_are_skipped() {
        let tmp = TempDir::new().unwrap();
        // Hidden dir with a manifest — should be skipped.
        let hidden = tmp.path().join(".hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(
            hidden.join("devcontainer-feature.json"),
            r#"{"id":"hidden"}"#,
        )
        .unwrap();
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        assert!(results.is_empty());
    }

    // -------------------------------------------------------------------------
    // VS Code extensions block
    // -------------------------------------------------------------------------

    #[test]
    fn vscode_extensions_rendered() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({
            "id": "feat",
            "version": "1.0.0",
            "customizations": {
                "vscode": {
                    "extensions": ["rust-lang.rust-analyzer", "tamasfe.even-better-toml"]
                }
            }
        });
        make_feature_dir(&tmp, "feat", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(readme.contains("## Customizations"), "got:\n{readme}");
        assert!(readme.contains("### VS Code Extensions"), "got:\n{readme}");
        assert!(
            readme.contains("- `rust-lang.rust-analyzer`"),
            "got:\n{readme}"
        );
        assert!(
            readme.contains("- `tamasfe.even-better-toml`"),
            "got:\n{readme}"
        );
    }

    // -------------------------------------------------------------------------
    // Deprecation header
    // -------------------------------------------------------------------------

    #[test]
    fn deprecated_feature_gets_important_note() {
        let tmp = TempDir::new().unwrap();
        let manifest = json!({
            "id": "old-feat",
            "version": "1.0.0",
            "deprecated": true,
            "legacyIds": ["feat-v1", "feat-old"]
        });
        make_feature_dir(&tmp, "old-feat", &manifest);
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(readme.contains("### **IMPORTANT NOTE**"), "got:\n{readme}");
        assert!(
            readme.contains("This Feature is deprecated"),
            "got:\n{readme}"
        );
        assert!(readme.contains("'feat-v1'"), "got:\n{readme}");
        assert!(readme.contains("'feat-old'"), "got:\n{readme}");
    }

    // -------------------------------------------------------------------------
    // JSONC manifest — comments and trailing commas accepted (regression)
    // -------------------------------------------------------------------------

    #[test]
    fn jsonc_manifest_with_comments_is_accepted() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("jsonc-feat");
        std::fs::create_dir_all(&dir).unwrap();
        // Write a manifest with line comments and a trailing comma — strict
        // serde_json would reject this; JSONC stripping must run first.
        std::fs::write(
            dir.join("devcontainer-feature.json"),
            r#"{
    // display name
    "id": "jsonc-feat",
    "name": "JSONC Feature",
    "version": "2.0.0", // trailing comma below
    "description": "has comments",
}"#,
        )
        .unwrap();
        let input = default_input(&tmp, "o/r");

        let results = generate_docs(&input).unwrap();
        assert_eq!(
            results.len(),
            1,
            "JSONC manifest must be parsed, not skipped"
        );
        let readme = std::fs::read_to_string(&results[0].readme_path).unwrap();
        assert!(
            readme.contains("# JSONC Feature (jsonc-feat)"),
            "got:\n{readme}"
        );
        assert!(readme.contains("has comments"), "got:\n{readme}");
    }

    // -------------------------------------------------------------------------
    // build_repo_url — URL normalisation (regression)
    // -------------------------------------------------------------------------

    #[test]
    fn footer_url_dot_project_folder_has_no_stray_dot_segment() {
        // When --project-folder is `.` (the default), the URL must not contain
        // a literal `./` or `/.` segment — it should go straight to the feature.
        let input_struct = GenerateDocsInput {
            project_folder: Path::new("."),
            registry: "ghcr.io",
            namespace: "o/r",
            github_owner: "owner",
            github_repo: "repo",
        };
        let url = build_repo_url("my-feat", &input_struct);
        assert!(
            !url.contains("/."),
            "URL must not contain a dot segment, got: {url}"
        );
        assert!(
            url.ends_with("/my-feat/devcontainer-feature.json"),
            "URL must end with feature path, got: {url}"
        );
        assert_eq!(
            url,
            "https://github.com/owner/repo/blob/main/my-feat/devcontainer-feature.json"
        );
    }

    #[test]
    fn footer_url_dot_slash_project_folder_normalized() {
        let input_struct = GenerateDocsInput {
            project_folder: Path::new("./src/features"),
            registry: "ghcr.io",
            namespace: "o/r",
            github_owner: "owner",
            github_repo: "repo",
        };
        let url = build_repo_url("my-feat", &input_struct);
        assert_eq!(
            url,
            "https://github.com/owner/repo/blob/main/src/features/my-feat/devcontainer-feature.json",
            "leading ./ must be stripped"
        );
    }

    #[test]
    fn footer_url_plain_subfolder() {
        let input_struct = GenerateDocsInput {
            project_folder: Path::new("features"),
            registry: "ghcr.io",
            namespace: "o/r",
            github_owner: "owner",
            github_repo: "repo",
        };
        let url = build_repo_url("node", &input_struct);
        assert_eq!(
            url,
            "https://github.com/owner/repo/blob/main/features/node/devcontainer-feature.json"
        );
    }
}
