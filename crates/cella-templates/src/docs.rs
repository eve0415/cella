//! Documentation generation for devcontainer template collections.
//!
//! Generates README.md files for each template in a `src/` directory,
//! matching the official `devcontainer templates generate-docs` output format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::types::{TemplateMetadata, TemplateOption};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by [`generate_docs`].
#[derive(Debug, Error)]
pub enum GenerateDocsError {
    /// Failed to read the `src/` directory.
    #[error("cannot read src directory at {path}: {source}")]
    ReadSrcDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An I/O error while writing a README.md.
    #[error("failed to write README for template '{id}': {source}")]
    WriteReadme {
        id: String,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Summary returned by [`generate_docs`].
#[derive(Debug, Default)]
pub struct GenerateDocsReport {
    /// Templates for which a README.md was successfully written.
    pub written: Vec<String>,

    /// Directories skipped (no `devcontainer-template.json` found).
    pub skipped: Vec<String>,

    /// Directories that failed to parse or write.
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Generate README.md files for all templates under `<project_folder>/src/`.
///
/// Each sub-directory of `src/` that contains a `devcontainer-template.json`
/// gets a freshly generated `README.md`.  Directories without the manifest
/// are skipped with a warning recorded in the report.
///
/// # Errors
///
/// Returns [`GenerateDocsError::ReadSrcDir`] when the `src/` directory cannot
/// be listed.  Per-template failures (parse errors, write failures) are
/// recorded in [`GenerateDocsReport::errors`] rather than aborting early.
pub fn generate_docs(
    project_folder: &Path,
    github_owner: Option<&str>,
    github_repo: Option<&str>,
) -> Result<GenerateDocsReport, GenerateDocsError> {
    let src_dir = project_folder.join("src");
    let entries = std::fs::read_dir(&src_dir).map_err(|e| GenerateDocsError::ReadSrcDir {
        path: src_dir.clone(),
        source: e,
    })?;

    let mut report = GenerateDocsReport::default();

    // Collect and sort for deterministic output (mirrors Promise.all ordering
    // in the official impl but sorts for reproducibility in tests).
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Skip hidden directories (mirrors the `!f.startsWith('.')` check in the official impl).
        if dir_name.starts_with('.') {
            continue;
        }

        process_template_dir(&dir, &dir_name, github_owner, github_repo, &mut report);
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Per-template helpers
// ---------------------------------------------------------------------------

fn process_template_dir(
    dir: &Path,
    dir_name: &str,
    github_owner: Option<&str>,
    github_repo: Option<&str>,
    report: &mut GenerateDocsReport,
) {
    let manifest_path = dir.join("devcontainer-template.json");

    if !manifest_path.exists() {
        eprintln!(
            "(!) Warning: devcontainer-template.json not found at path '{}'. Skipping...",
            manifest_path.display()
        );
        report.skipped.push(dir_name.to_owned());
        return;
    }

    let raw = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read {}: {e}", manifest_path.display());
            report.errors.push(dir_name.to_owned());
            return;
        }
    };

    let metadata: TemplateMetadata = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to parse {}: {e}", manifest_path.display());
            report.errors.push(dir_name.to_owned());
            return;
        }
    };

    let readme_content = render_readme(&metadata, dir, github_owner, github_repo);
    let readme_path = dir.join("README.md");

    match std::fs::write(&readme_path, readme_content) {
        Ok(()) => {
            eprintln!("Generating {}...", readme_path.display());
            report.written.push(metadata.id);
        }
        Err(e) => {
            // Surface the error in the report but don't abort.
            report.errors.push(format!("{dir_name} (write error: {e})"));
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Build the full README.md content for a single template.
fn render_readme(
    metadata: &TemplateMetadata,
    template_dir: &Path,
    github_owner: Option<&str>,
    github_repo: Option<&str>,
) -> String {
    let name_line = build_name_line(metadata);
    let description = metadata.description.as_deref().unwrap_or_default();
    let options_section = build_options_section(&metadata.options);
    let notes = read_notes(template_dir);
    let repo_url = build_repo_url(template_dir, github_owner, github_repo);

    format!(
        "\n# {name_line}\n\n{description}\n\n{options_section}\n\n{notes}\n\n---\n\n_Note: This file was auto-generated from the [devcontainer-template.json]({repo_url}).  Add additional notes to a `NOTES.md`._\n"
    )
}

/// `Name (id)` when a name field exists, otherwise just `id`.
fn build_name_line(metadata: &TemplateMetadata) -> String {
    metadata.name.as_ref().map_or_else(
        || metadata.id.clone(),
        |name| format!("{name} ({})", metadata.id),
    )
}

/// Build the `## Options` table or an empty string when there are no options.
///
/// Option key order is sorted for deterministic output.
fn build_options_section(options: &std::collections::HashMap<String, TemplateOption>) -> String {
    if options.is_empty() {
        return String::new();
    }

    // Sort keys for stable output.
    let sorted: BTreeMap<&str, &TemplateOption> =
        options.iter().map(|(k, v)| (k.as_str(), v)).collect();

    let rows: Vec<String> = sorted
        .iter()
        .map(|(key, opt)| build_option_row(key, opt))
        .collect();

    format!(
        "## Options\n\n| Options Id | Description | Type | Default Value |\n|-----|-----|-----|-----|\n{}",
        rows.join("\n")
    )
}

/// Render a single table row for one option.
fn build_option_row(key: &str, opt: &TemplateOption) -> String {
    let desc = opt.description.as_deref().unwrap_or("-");
    let typ = opt.option_type.as_str();
    let default = default_display(&opt.default);
    format!("| {key} | {desc} | {typ} | {default} |")
}

/// Convert a `serde_json::Value` default to a display string.
fn default_display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) if s.is_empty() => "-".to_owned(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "-".to_owned(),
        other => other.to_string(),
    }
}

/// Read `NOTES.md` from the template directory, or return an empty string.
fn read_notes(template_dir: &Path) -> String {
    let notes_path = template_dir.join("NOTES.md");
    if notes_path.exists() {
        std::fs::read_to_string(&notes_path).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Build the URL for the `devcontainer-template.json` link in the README footer.
///
/// When both `github_owner` and `github_repo` are provided, returns a full
/// GitHub URL pointing to `src/<id>/devcontainer-template.json` on the main
/// branch.  Otherwise returns the bare filename.
fn build_repo_url(
    template_dir: &Path,
    github_owner: Option<&str>,
    github_repo: Option<&str>,
) -> String {
    match (github_owner, github_repo) {
        (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => {
            let dir_name = template_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            format!(
                "https://github.com/{owner}/{repo}/blob/main/src/{dir_name}/devcontainer-template.json"
            )
        }
        _ => "devcontainer-template.json".to_owned(),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::types::TemplateOption;

    fn write_manifest(dir: &Path, json: &str) {
        std::fs::write(dir.join("devcontainer-template.json"), json).unwrap();
    }

    fn write_notes(dir: &Path, content: &str) {
        std::fs::write(dir.join("NOTES.md"), content).unwrap();
    }

    fn read_readme(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("README.md")).unwrap()
    }

    // -----------------------------------------------------------------------
    // Test 1: template with string + boolean options and NOTES.md
    // -----------------------------------------------------------------------

    #[test]
    fn generates_readme_with_options_and_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let tpl_dir = src.join("rust");
        std::fs::create_dir_all(&tpl_dir).unwrap();

        write_manifest(
            &tpl_dir,
            r#"{
                "id": "rust",
                "version": "1.0.0",
                "name": "Rust",
                "description": "Develop Rust applications.",
                "options": {
                    "imageVariant": {
                        "type": "string",
                        "description": "Debian OS version",
                        "proposals": ["trixie", "bookworm"],
                        "default": "trixie"
                    },
                    "installTools": {
                        "type": "boolean",
                        "description": "Install extra tools",
                        "default": false
                    }
                }
            }"#,
        );
        write_notes(&tpl_dir, "## Extra Notes\n\nSome extra content.\n");

        generate_docs(tmp.path(), Some("myorg"), Some("myrepo")).unwrap();

        let readme = read_readme(&tpl_dir);

        // Title: name (id) form
        assert!(readme.contains("# Rust (rust)"), "title line missing");

        // Description
        assert!(
            readme.contains("Develop Rust applications."),
            "description missing"
        );

        // Options table header
        assert!(
            readme.contains("| Options Id | Description | Type | Default Value |"),
            "options table header missing"
        );

        // Option rows (sorted: imageVariant before installTools)
        assert!(
            readme.contains("| imageVariant | Debian OS version | string | trixie |"),
            "imageVariant row missing"
        );
        assert!(
            readme.contains("| installTools | Install extra tools | boolean | false |"),
            "installTools row missing"
        );

        // NOTES.md injection
        assert!(
            readme.contains("## Extra Notes"),
            "NOTES.md content not injected"
        );

        // Footer with GitHub link
        assert!(
            readme.contains(
                "https://github.com/myorg/myrepo/blob/main/src/rust/devcontainer-template.json"
            ),
            "GitHub footer URL missing"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: directory missing manifest → skipped with warning in report
    // -----------------------------------------------------------------------

    #[test]
    fn skips_directory_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("no-manifest")).unwrap();

        let report = generate_docs(tmp.path(), None, None).unwrap();

        assert_eq!(report.skipped, vec!["no-manifest"]);
        assert!(report.written.is_empty());
        assert!(report.errors.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 3: overwrite behavior → existing README.md gets replaced
    // -----------------------------------------------------------------------

    #[test]
    fn overwrites_existing_readme() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let tpl_dir = src.join("alpine");
        std::fs::create_dir_all(&tpl_dir).unwrap();

        std::fs::write(tpl_dir.join("README.md"), "old content").unwrap();

        write_manifest(
            &tpl_dir,
            r#"{"id": "alpine", "version": "1.0.0", "name": "Alpine", "description": "A minimal Alpine container."}"#,
        );

        generate_docs(tmp.path(), None, None).unwrap();

        let readme = read_readme(&tpl_dir);
        assert!(
            !readme.contains("old content"),
            "old README should be replaced"
        );
        assert!(
            readme.contains("alpine"),
            "new README should contain template id"
        );
    }

    // -----------------------------------------------------------------------
    // Unit helpers
    // -----------------------------------------------------------------------

    #[test]
    fn name_line_with_name_field() {
        let meta = TemplateMetadata {
            id: "rust".to_owned(),
            version: "1.0.0".to_owned(),
            name: Some("Rust".to_owned()),
            description: None,
            documentation_url: None,
            license_url: None,
            publisher: None,
            platforms: vec![],
            keywords: vec![],
            optional_paths: vec![],
            options: HashMap::new(),
        };
        assert_eq!(build_name_line(&meta), "Rust (rust)");
    }

    #[test]
    fn name_line_without_name_field() {
        let meta = TemplateMetadata {
            id: "bare".to_owned(),
            version: "1.0.0".to_owned(),
            name: None,
            description: None,
            documentation_url: None,
            license_url: None,
            publisher: None,
            platforms: vec![],
            keywords: vec![],
            optional_paths: vec![],
            options: HashMap::new(),
        };
        assert_eq!(build_name_line(&meta), "bare");
    }

    #[test]
    fn default_display_empty_string_becomes_dash() {
        assert_eq!(
            default_display(&serde_json::Value::String(String::new())),
            "-"
        );
    }

    #[test]
    fn options_section_empty_when_no_options() {
        assert_eq!(build_options_section(&HashMap::new()), "");
    }

    #[test]
    fn options_section_sorted_keys() {
        let mut opts = HashMap::new();
        opts.insert(
            "zzz".to_owned(),
            TemplateOption {
                option_type: "string".to_owned(),
                description: Some("last".to_owned()),
                default: serde_json::json!("z"),
                proposals: None,
                enum_values: None,
            },
        );
        opts.insert(
            "aaa".to_owned(),
            TemplateOption {
                option_type: "boolean".to_owned(),
                description: Some("first".to_owned()),
                default: serde_json::json!(true),
                proposals: None,
                enum_values: None,
            },
        );

        let section = build_options_section(&opts);
        let aaa_pos = section.find("| aaa |").unwrap();
        let zzz_pos = section.find("| zzz |").unwrap();
        assert!(aaa_pos < zzz_pos, "options must be sorted by key");
    }
}
