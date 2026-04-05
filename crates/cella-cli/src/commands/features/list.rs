//! `cella features list` — show configured or available features.

use clap::Args;

use cella_templates::cache::TemplateCache;
use cella_templates::collection::{self, DEFAULT_FEATURE_COLLECTION};

use super::resolve::{self, CommonFeatureFlags};
use crate::commands::OutputFormat;

/// List configured or available devcontainer features.
#[derive(Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub common: CommonFeatureFlags,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,

    /// Show available features from the registry instead of configured ones.
    #[arg(long)]
    pub available: bool,

    /// Force re-fetch collection index (ignore cache).
    #[arg(long)]
    pub refresh: bool,
}

impl ListArgs {
    /// Execute the list command.
    ///
    /// # Errors
    ///
    /// Returns error on config discovery failure or network errors.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.available {
            self.list_available().await
        } else {
            self.list_configured().await
        }
    }

    async fn list_configured(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config_path = resolve::discover_config(&self.common)?;
        let raw = resolve::read_raw_config(&config_path)?;

        // Strip JSONC comments and parse
        let stripped = cella_jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;
        let features = resolve::extract_features(&config);

        if features.is_empty() {
            if matches!(self.output, OutputFormat::Json) {
                println!("[]");
            } else {
                eprintln!("No features configured.");
            }
            return Ok(());
        }

        if matches!(self.output, OutputFormat::Json) {
            let json_features: Vec<serde_json::Value> = features
                .iter()
                .map(|(reference, options)| {
                    serde_json::json!({
                        "reference": reference,
                        "options": options,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_features)?);
            return Ok(());
        }

        // Resolve display names
        let cache = TemplateCache::new();
        let mut display_rows: Vec<(String, String, String)> = Vec::new();
        for (reference, options) in &features {
            let name = resolve::resolve_feature_name(reference, &cache).await;
            let opts_str = format_options(options);
            display_rows.push((reference.clone(), name, opts_str));
        }

        eprintln!("Configured features:");
        for (reference, name, opts) in &display_rows {
            if opts.is_empty() {
                eprintln!("  {reference:<50} {name}");
            } else {
                eprintln!("  {reference:<50} {name:<20} {opts}");
            }
        }

        Ok(())
    }

    async fn list_available(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let registry = self
            .common
            .registry
            .as_deref()
            .unwrap_or(DEFAULT_FEATURE_COLLECTION);
        let cache = TemplateCache::new();
        let collection =
            collection::fetch_feature_collection(registry, &cache, self.refresh).await?;

        if collection.features.is_empty() {
            if matches!(self.output, OutputFormat::Json) {
                println!("[]");
            } else {
                eprintln!("No features found in {registry}.");
            }
            return Ok(());
        }

        if matches!(self.output, OutputFormat::Json) {
            let json_features: Vec<serde_json::Value> = collection
                .features
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "id": f.id,
                        "version": f.version,
                        "name": f.name,
                        "description": f.description,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_features)?);
            return Ok(());
        }

        eprintln!("Available features ({registry}):");
        for feature in &collection.features {
            let name = feature.name.as_deref().unwrap_or(&feature.id);
            let desc = feature
                .description
                .as_deref()
                .map(|d| truncate(d, 50))
                .unwrap_or_default();
            let ref_str = format!("{registry}/{}:{}", feature.id, feature.version);
            eprintln!("  {name:<25} {desc:<52} {ref_str}");
        }

        Ok(())
    }
}

/// Format feature options as a compact string.
fn format_options(options: &serde_json::Value) -> String {
    let Some(obj) = options.as_object() else {
        return String::new();
    };
    if obj.is_empty() {
        return String::new();
    }
    obj.iter()
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k}={val}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Truncate a string to max length, adding "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_options_empty() {
        assert_eq!(format_options(&serde_json::json!({})), "");
    }

    #[test]
    fn format_options_single() {
        assert_eq!(
            format_options(&serde_json::json!({"version": "lts"})),
            "version=lts"
        );
    }

    #[test]
    fn format_options_boolean() {
        let result = format_options(&serde_json::json!({"debug": true}));
        assert_eq!(result, "debug=true");
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long() {
        let result = truncate("this is a very long description", 15);
        assert_eq!(result, "this is a ve...");
    }

    #[test]
    fn truncate_exact_boundary() {
        let result = truncate("hello", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_one_over() {
        let result = truncate("abcdef", 5);
        assert_eq!(result, "ab...");
    }

    #[test]
    fn format_options_non_object() {
        assert_eq!(format_options(&serde_json::Value::Null), "");
        assert_eq!(format_options(&serde_json::json!("string")), "");
        assert_eq!(format_options(&serde_json::json!([1, 2])), "");
    }

    #[test]
    fn format_options_multiple() {
        let opts = serde_json::json!({"version": "lts", "debug": true});
        let result = format_options(&opts);
        // Order is not guaranteed in JSON objects, check both parts
        assert!(result.contains("version=lts"));
        assert!(result.contains("debug=true"));
        assert!(result.contains(", "));
    }
}
