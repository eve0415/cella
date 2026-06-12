use std::collections::HashMap;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::Value;

use cella_templates::{SelectedFeature, TemplateError, apply, fetcher, options};

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
    pub log_level: TemplatesLogLevel,
}

/// Log level for the `templates apply` subcommand.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum TemplatesLogLevel {
    Info,
    Debug,
    Trace,
}

impl TemplatesArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            TemplatesCommand::Apply(args) => args.execute().await,
            TemplatesCommand::New { .. }
            | TemplatesCommand::List
            | TemplatesCommand::Edit { .. } => {
                eprintln!("cella templates: not yet implemented");
                Err("not yet implemented".into())
            }
        }
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
            inject_features(&workspace, &features)?;
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

/// Parse `--features` JSON array: each entry must have an `"id"` string field.
fn parse_features_json(
    raw: &str,
) -> Result<Vec<SelectedFeature>, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|e| format!("--features: invalid JSON: {e}"))?;

    let Value::Array(arr) = parsed else {
        return Err("--features: must be a JSON array".into());
    };

    let mut result = Vec::with_capacity(arr.len());
    for (idx, item) in arr.into_iter().enumerate() {
        let Value::Object(obj) = item else {
            return Err(format!("--features[{idx}]: must be a JSON object").into());
        };

        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("--features[{idx}]: missing required string field \"id\""))?
            .to_owned();

        let feature_options: HashMap<String, Value> = obj
            .get("options")
            .and_then(Value::as_object)
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        result.push(SelectedFeature {
            reference: id,
            options: feature_options,
        });
    }

    Ok(result)
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
fn inject_features(
    workspace: &std::path::Path,
    features: &[SelectedFeature],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Look for .devcontainer/devcontainer.json or devcontainer.json at root.
    let candidates = [
        workspace.join(".devcontainer").join("devcontainer.json"),
        workspace.join("devcontainer.json"),
    ];

    let config_path = candidates.iter().find(|p| p.exists()).ok_or_else(
        || -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(TemplateError::InvalidArtifact {
                template_id: "apply".to_owned(),
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
        assert!(
            err.to_string()
                .contains("missing required string field \"id\"")
        );
    }

    #[test]
    fn features_rejects_non_object_entry() {
        let err = parse_features_json(r#"["not-an-object"]"#).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
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
}
