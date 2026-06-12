use std::collections::HashMap;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::Value;

use cella_templates::{
    SelectedFeature, TemplateError, apply, fetcher, generate_docs, metadata, options,
    publish::{PublishOptions, publish_templates},
};

use super::LogLevel;

/// Manage dev container templates.
#[derive(Args)]
pub struct TemplatesArgs {
    #[command(subcommand)]
    pub command: TemplatesCommand,
}

#[derive(Subcommand)]
pub enum TemplatesCommand {
    /// Apply a template to a workspace folder.
    Apply(TemplatesApplyArgs),
    /// Fetch a published template's metadata from its OCI manifest.
    Metadata(TemplatesMetadataArgs),
    /// Generate documentation (README.md) for all templates in a collection.
    GenerateDocs(TemplatesGenerateDocsArgs),
    /// Publish templates (single or collection) to an OCI registry.
    Publish(TemplatesPublishArgs),
    /// Create a new template.
    New {
        /// Name for the template.
        name: String,
    },
    /// List available templates.
    List,
    /// Edit an existing template.
    Edit {
        /// Name of the template to edit.
        name: String,
    },
}

/// Arguments for `cella templates publish`.
///
/// Flag surface matches `devcontainer templates publish` exactly.
#[derive(Args)]
pub struct TemplatesPublishArgs {
    /// Path to a single template directory or a collection directory.
    /// Defaults to the current directory.
    #[arg(default_value = ".")]
    pub target: PathBuf,

    /// OCI registry to publish to.
    #[arg(short = 'r', long = "registry", default_value = "ghcr.io")]
    pub registry: String,

    /// OCI namespace (owner/org) under which templates are published.
    #[arg(short = 'n', long = "namespace", required = true)]
    pub namespace: String,

    /// Log verbosity level.
    #[arg(long = "log-level", value_enum, default_value = "info")]
    pub log_level: LogLevel,
}

/// Arguments for `cella templates generate-docs`.
///
/// Flag surface matches `devcontainer templates generate-docs` exactly.
#[derive(Args)]
pub struct TemplatesGenerateDocsArgs {
    /// Path to the folder containing a `src/` sub-folder with template directories.
    /// Defaults to the current directory.
    #[arg(short = 'p', long = "project-folder", default_value = ".")]
    pub project_folder: PathBuf,

    /// GitHub owner name used to build the README footer URL.
    #[arg(long = "github-owner", default_value = "")]
    pub github_owner: String,

    /// GitHub repository name used to build the README footer URL.
    #[arg(long = "github-repo", default_value = "")]
    pub github_repo: String,
}

/// Arguments for `cella templates metadata`.
///
/// Flag surface matches `devcontainer templates metadata` exactly.
#[derive(Args)]
pub struct TemplatesMetadataArgs {
    /// Template OCI reference (e.g. ghcr.io/devcontainers/templates/alpine).
    ///
    /// Optional: the official CLI treats a missing/unresolvable ref as a
    /// not-found result — it prints `{}` to stdout, warns, and exits 1 —
    /// rather than erroring out with a clap usage message.
    pub template_id: Option<String>,

    /// Log verbosity level.
    #[arg(long = "log-level", value_enum, default_value = "info")]
    pub log_level: LogLevel,
}

/// Arguments for `cella templates apply`.
///
/// Flag surface matches `devcontainer templates apply` exactly.
#[derive(Args)]
pub struct TemplatesApplyArgs {
    /// Workspace folder to apply the template into (default: current directory).
    #[arg(short = 'w', long)]
    pub workspace_folder: Option<PathBuf>,

    /// Template OCI reference (e.g. ghcr.io/devcontainers/templates/rust).
    #[arg(short = 't', long = "template-id", required = true)]
    pub template_id: String,

    /// Template arguments as a JSON object string (`{"key": "value"}`).
    /// All values must be strings.
    #[arg(short = 'a', long = "template-args", default_value = "{}")]
    pub template_args: String,

    /// Features to inject as a JSON array (`[{"id": "...", "options": {...}}]`).
    #[arg(short = 'f', long = "features", default_value = "[]")]
    pub features: String,

    /// Paths/globs to omit from extraction, as a JSON array (`["path", ...]`).
    #[arg(long = "omit-paths", default_value = "[]")]
    pub omit_paths: String,

    /// Temporary directory for intermediate work (default: system temp).
    #[arg(long = "tmp-dir")]
    pub tmp_dir: Option<PathBuf>,

    /// Log verbosity level.
    #[arg(long = "log-level", value_enum, default_value = "info")]
    pub log_level: LogLevel,
}

impl TemplatesArgs {
    /// Return the `--log-level` from whichever subcommand carries one (`apply`
    /// or `metadata`), if active.
    ///
    /// Called by [`super::Command::log_level`] so the global tracing filter is
    /// seeded before dispatch — the same pattern used by `up` and
    /// `run-user-commands`.
    pub const fn apply_log_level(&self) -> Option<LogLevel> {
        match &self.command {
            TemplatesCommand::Apply(args) => Some(args.log_level),
            TemplatesCommand::Metadata(args) => Some(args.log_level),
            TemplatesCommand::GenerateDocs(_)
            | TemplatesCommand::New { .. }
            | TemplatesCommand::List
            | TemplatesCommand::Edit { .. } => None,
        }
    }

    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            TemplatesCommand::Apply(args) => args.execute().await,
            TemplatesCommand::Metadata(args) => args.execute().await,
            TemplatesCommand::GenerateDocs(args) => args.execute(),
            TemplatesCommand::Publish(args) => args.execute().await,
            TemplatesCommand::New { .. }
            | TemplatesCommand::List
            | TemplatesCommand::Edit { .. } => {
                eprintln!("cella templates: not yet implemented");
                Err("not yet implemented".into())
            }
        }
    }
}

impl TemplatesPublishArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let opts = PublishOptions {
            target: self.target,
            registry: self.registry,
            namespace: self.namespace,
        };

        let output = publish_templates(opts).await?;
        println!(
            "{}",
            serde_json::to_string(&output).expect("output is serializable")
        );
        Ok(())
    }
}

impl TemplatesApplyArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let workspace =
            crate::commands::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        // Parse --template-args: must be a JSON object with string values only.
        let template_args = parse_template_args(&self.template_args)?;

        // Parse --features: JSON array of {id, options?} objects.
        let features = parse_features_json(&self.features)?;

        // Parse --omit-paths: JSON array of strings.
        let omit_paths = parse_string_array(&self.omit_paths, "--omit-paths")?;

        // Use --tmp-dir as cache root when provided, otherwise use the default.
        let cache = self
            .tmp_dir
            .as_ref()
            .map_or_else(cella_templates::TemplateCache::new, |tmp| {
                cella_templates::TemplateCache::with_root(tmp)
            });

        // Fetch template artifact.
        let template_dir = fetcher::fetch_template(&self.template_id, &cache).await?;

        // Read metadata and resolve options.
        let metadata = fetcher::read_template_metadata(&template_dir)?;
        let mut resolved_opts =
            options::resolve_options(&metadata.id, &metadata.options, &template_args)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Pass undeclared user keys through to substitution unchanged.
        // Official behavior: the user-supplied map is the base; resolve_options
        // only fills *declared* options that are absent from it. Extra keys the
        // user supplies for custom tokens must still reach substitution so that
        // `${templateOption:custom}` is replaced rather than left empty.
        for (key, value) in &template_args {
            resolved_opts
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }

        // Apply template to workspace root.
        let written = apply::apply_to_workspace(
            &metadata.id,
            &template_dir,
            &workspace,
            &resolved_opts,
            &omit_paths,
        )
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Inject features into extracted devcontainer.json if requested.
        if !features.is_empty() {
            inject_features(&workspace, &features, &metadata.id)?;
        }

        // Print success JSON to stdout (official contract).
        let file_strs: Vec<String> = written
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let output = serde_json::json!({"files": file_strs});
        println!("{}", serde_json::to_string(&output).expect("serializable"));

        Ok(())
    }
}

impl TemplatesMetadataArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Official behavior: a missing or unresolvable ref is treated as a
        // not-found result — print `{}` to stdout, warn on stderr, exit 1.
        let Some(template_id) = self.template_id else {
            eprintln!("error: no template identifier provided");
            println!("{{}}");
            std::process::exit(1);
        };

        match metadata::fetch_manifest_metadata(&template_id).await {
            Ok(Some(raw)) => match render_metadata_annotation(&raw) {
                Ok(json) => {
                    println!("{json}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("warning: metadata annotation is not valid JSON: {e}");
                    println!("{{}}");
                    std::process::exit(1);
                }
            },
            Ok(None) => {
                eprintln!(
                    "Template resolved to '{template_id}' but does not contain metadata on its manifest."
                );
                eprintln!(
                    "Ask the Template owner to republish this Template to populate the manifest."
                );
                println!("{{}}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                println!("{{}}");
                std::process::exit(1);
            }
        }
    }
}

impl TemplatesGenerateDocsArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let project_folder = if self.project_folder == std::path::Path::new(".") {
            std::env::current_dir()?
        } else {
            self.project_folder.clone()
        };

        let owner = (!self.github_owner.is_empty()).then_some(self.github_owner.as_str());
        let repo = (!self.github_repo.is_empty()).then_some(self.github_repo.as_str());

        let report = generate_docs(&project_folder, owner, repo)?;

        for id in &report.written {
            eprintln!("  wrote: src/{id}/README.md");
        }
        for dir in &report.skipped {
            eprintln!("  skipped: {dir} (no devcontainer-template.json)");
        }
        for dir in &report.errors {
            // Official behavior: log per-template failures and continue — exit 0.
            eprintln!("  error: {dir}");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Re-parse and re-serialize the raw metadata annotation string so the output
/// is compact valid JSON rather than the annotation's escaped string value.
///
/// Returns `Err` when the annotation is not valid JSON.
fn render_metadata_annotation(raw: &str) -> Result<String, serde_json::Error> {
    let parsed: Value = serde_json::from_str(raw)?;
    Ok(serde_json::to_string(&parsed).expect("re-serializing a Value is infallible"))
}

// ---------------------------------------------------------------------------
// Argument parsers
// ---------------------------------------------------------------------------

/// Parse `--template-args` JSON: must be an object with only string values.
fn parse_template_args(
    raw: &str,
) -> Result<HashMap<String, Value>, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|e| format!("--template-args: invalid JSON: {e}"))?;

    let Value::Object(map) = parsed else {
        return Err("--template-args: must be a JSON object".into());
    };

    for (key, val) in &map {
        if !val.is_string() {
            return Err(format!(
                "--template-args: value for key \"{key}\" must be a string, got {val}"
            )
            .into());
        }
    }

    Ok(map.into_iter().collect())
}

/// Wire format for a single entry in the `--features` JSON array.
///
/// Official input shape: `[{"id": "...", "options": {...}}]`.
/// `options` is optional — an absent key is treated as an empty map.
#[derive(serde::Deserialize)]
struct FeatureEntry {
    /// Full OCI reference for the feature (required).
    id: String,
    /// Option key-value pairs for this feature (optional).
    #[serde(default)]
    options: HashMap<String, Value>,
}

/// Parse `--features` JSON array using serde.
///
/// Each entry must be an object with a required `"id"` string field.
/// Returns a clear miette-compatible error message on missing id or bad shape.
fn parse_features_json(
    raw: &str,
) -> Result<Vec<SelectedFeature>, Box<dyn std::error::Error + Send + Sync>> {
    // First check the top level is an array so the error message names `--features`.
    let top: Value =
        serde_json::from_str(raw).map_err(|e| format!("--features: invalid JSON: {e}"))?;
    if !top.is_array() {
        return Err("--features: must be a JSON array".into());
    }

    // Deserialize into the wire type; serde gives us the missing-id error automatically.
    let entries: Vec<FeatureEntry> =
        serde_json::from_value(top).map_err(|e| format!("--features: invalid entry: {e}"))?;

    Ok(entries
        .into_iter()
        .map(|e| SelectedFeature {
            reference: e.id,
            options: e.options,
        })
        .collect())
}

/// Parse a `--flag` JSON array of strings.
fn parse_string_array(
    raw: &str,
    flag: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|e| format!("{flag}: invalid JSON: {e}"))?;

    let Value::Array(arr) = parsed else {
        return Err(format!("{flag}: must be a JSON array").into());
    };

    arr.into_iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag}[{i}]: must be a string").into())
        })
        .collect()
}

/// Inject features into the extracted devcontainer.json.
///
/// # Comment-loss divergence
///
/// The official CLI uses `jsonc.modify` to patch individual `features.<id>`
/// keys in-place, which preserves comments and existing formatting. The
/// `cella-jsonc` crate only exposes a comment-stripping function (`strip`), so
/// this path strips JSONC before parsing and rewrites as plain JSON. Comments
/// that existed in the extracted devcontainer.json are therefore lost when
/// `--features` is supplied.
///
/// This is a known divergence. A comment-preserving path would require a
/// JSONC AST-edit capability not yet available in `cella-jsonc`.
fn inject_features(
    workspace: &std::path::Path,
    features: &[SelectedFeature],
    template_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Look for .devcontainer/devcontainer.json or devcontainer.json at root.
    let candidates = [
        workspace.join(".devcontainer").join("devcontainer.json"),
        workspace.join("devcontainer.json"),
    ];

    let config_path = candidates.iter().find(|p| p.exists()).ok_or_else(
        || -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(TemplateError::InvalidArtifact {
                template_id: template_id.to_owned(),
                reason: "no devcontainer.json found after template extraction".to_owned(),
            })
        },
    )?;

    let raw = std::fs::read_to_string(config_path)?;
    let stripped =
        cella_jsonc::strip(&raw).map_err(|e| format!("devcontainer.json: JSONC error: {e}"))?;
    let mut config: Value = serde_json::from_str(&stripped)
        .map_err(|e| format!("devcontainer.json: invalid JSON: {e}"))?;

    apply::merge_features(&mut config, features);

    let formatted = serde_json::to_string_pretty(&config).expect("config is serializable") + "\n";
    std::fs::write(config_path, formatted)?;

    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_template_args
    // -----------------------------------------------------------------------

    #[test]
    fn template_args_empty_object() {
        let result = parse_template_args("{}").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn template_args_string_values() {
        let result = parse_template_args(r#"{"variant": "trixie", "debug": "false"}"#).unwrap();
        assert_eq!(result["variant"], Value::String("trixie".to_owned()));
        assert_eq!(result["debug"], Value::String("false".to_owned()));
    }

    #[test]
    fn template_args_rejects_non_string_value() {
        let err = parse_template_args(r#"{"variant": 42}"#).unwrap_err();
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn template_args_rejects_array() {
        let err = parse_template_args(r#"["a", "b"]"#).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn template_args_rejects_invalid_json() {
        let err = parse_template_args("{bad json}").unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
    }

    // -----------------------------------------------------------------------
    // parse_features_json
    // -----------------------------------------------------------------------

    #[test]
    fn features_empty_array() {
        let result = parse_features_json("[]").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn features_single_entry() {
        let result =
            parse_features_json(r#"[{"id": "ghcr.io/devcontainers/features/node:1"}]"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].reference, "ghcr.io/devcontainers/features/node:1");
        assert!(result[0].options.is_empty());
    }

    #[test]
    fn features_with_options() {
        let result = parse_features_json(
            r#"[{"id": "ghcr.io/devcontainers/features/node:1", "options": {"version": "lts"}}]"#,
        )
        .unwrap();
        assert_eq!(
            result[0].options["version"],
            Value::String("lts".to_owned())
        );
    }

    #[test]
    fn features_rejects_missing_id() {
        let err = parse_features_json(r#"[{"options": {}}]"#).unwrap_err();
        // serde produces "missing field `id`" for a missing required field.
        let msg = err.to_string();
        assert!(
            msg.contains("id"),
            "expected error mentioning missing 'id' field, got: {msg}"
        );
    }

    #[test]
    fn features_rejects_non_object_entry() {
        let err = parse_features_json(r#"["not-an-object"]"#).unwrap_err();
        // serde produces "invalid type: string …, expected struct FeatureEntry"
        let msg = err.to_string();
        assert!(
            msg.contains("invalid"),
            "expected invalid-type error, got: {msg}"
        );
    }

    #[test]
    fn features_rejects_non_array() {
        let err = parse_features_json(r#"{"id": "x"}"#).unwrap_err();
        assert!(err.to_string().contains("must be a JSON array"));
    }

    // -----------------------------------------------------------------------
    // parse_string_array
    // -----------------------------------------------------------------------

    #[test]
    fn string_array_empty() {
        let result = parse_string_array("[]", "--omit-paths").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn string_array_values() {
        let result = parse_string_array(r#"[".github/*", "docs/*"]"#, "--omit-paths").unwrap();
        assert_eq!(result, vec![".github/*", "docs/*"]);
    }

    #[test]
    fn string_array_rejects_non_string() {
        let err = parse_string_array("[42]", "--omit-paths").unwrap_err();
        assert!(err.to_string().contains("must be a string"));
    }

    // -----------------------------------------------------------------------
    // TemplatesCommand stubs still return not-implemented
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn new_returns_not_implemented() {
        let args = TemplatesArgs {
            command: TemplatesCommand::New {
                name: "rust".to_owned(),
            },
        };
        let result = args.execute().await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "not yet implemented");
    }

    #[tokio::test]
    async fn list_returns_not_implemented() {
        let args = TemplatesArgs {
            command: TemplatesCommand::List,
        };
        let result = args.execute().await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // TemplatesMetadataArgs
    // -----------------------------------------------------------------------

    /// Wrapper so we can drive clap parsing of the metadata subcommand in unit
    /// tests without reaching into the binary's top-level `Cli`.
    #[derive(clap::Parser)]
    struct MetadataHarness {
        #[command(subcommand)]
        command: TemplatesCommand,
    }

    fn parse_metadata(argv: &[&str]) -> Result<TemplatesMetadataArgs, clap::Error> {
        let mut full = vec!["templates", "metadata"];
        full.extend_from_slice(argv);
        let parsed = <MetadataHarness as clap::Parser>::try_parse_from(full)?;
        match parsed.command {
            TemplatesCommand::Metadata(args) => Ok(args),
            _ => unreachable!("parsed the metadata subcommand"),
        }
    }

    #[test]
    fn metadata_args_captures_template_id() {
        let args = parse_metadata(&["ghcr.io/devcontainers/templates/alpine"]).unwrap();
        assert_eq!(
            args.template_id.as_deref(),
            Some("ghcr.io/devcontainers/templates/alpine")
        );
    }

    #[test]
    fn metadata_template_id_is_optional() {
        // Regression: a missing template id must NOT make clap exit with a usage
        // error. It parses to `None`, and the command then prints `{}`/exits 1
        // at runtime (matching the official "unresolvable ref" contract).
        let args = parse_metadata(&[]).unwrap();
        assert!(args.template_id.is_none());
    }

    #[test]
    fn metadata_accepts_log_level_flag() {
        // Regression: the metadata subcommand must accept `--log-level`,
        // matching the official CLI's registered flag surface.
        for level in ["info", "debug", "trace"] {
            let args = parse_metadata(&[
                "ghcr.io/devcontainers/templates/alpine",
                "--log-level",
                level,
            ])
            .unwrap_or_else(|e| panic!("--log-level {level} should parse: {e}"));
            assert!(args.template_id.is_some());
        }
    }

    #[test]
    fn metadata_log_level_defaults_to_info() {
        let args = parse_metadata(&["ghcr.io/devcontainers/templates/alpine"]).unwrap();
        assert!(matches!(args.log_level, LogLevel::Info));
    }

    #[test]
    fn metadata_log_level_wired_through_apply_log_level() {
        let parsed = <MetadataHarness as clap::Parser>::try_parse_from([
            "templates",
            "metadata",
            "ghcr.io/devcontainers/templates/alpine",
            "--log-level",
            "debug",
        ])
        .unwrap();
        let templates_args = TemplatesArgs {
            command: parsed.command,
        };
        assert!(matches!(
            templates_args.apply_log_level(),
            Some(LogLevel::Debug)
        ));
    }

    // -----------------------------------------------------------------------
    // render_metadata_annotation
    // -----------------------------------------------------------------------

    #[test]
    fn render_valid_annotation_produces_compact_json() {
        let raw = r#"{"id":"alpine","version":"1.0.0"}"#;
        let result = render_metadata_annotation(raw).unwrap();
        // Re-serialization should produce valid compact JSON.
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["id"], Value::String("alpine".to_owned()));
        assert_eq!(v["version"], Value::String("1.0.0".to_owned()));
    }

    #[test]
    fn render_invalid_annotation_returns_err() {
        let result = render_metadata_annotation("{not valid json}");
        assert!(result.is_err());
    }

    #[test]
    fn render_annotation_with_whitespace_produces_compact_output() {
        // Annotation may have extra whitespace or pretty-printing; output must be compact.
        let raw = "{\n  \"id\": \"alpine\"\n}";
        let result = render_metadata_annotation(raw).unwrap();
        assert!(
            !result.contains('\n'),
            "output should be compact (no newlines)"
        );
    }
}
