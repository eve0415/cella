//! `cella features edit` — unified feature editor with interactive and
//! non-interactive modes.

use std::collections::HashMap;

use clap::Args;
use inquire::Select;

use cella_templates::cache::TemplateCache;
use cella_templates::collection::{self, DEFAULT_FEATURE_COLLECTION};
use cella_templates::fetcher;
use cella_templates::types::SelectedFeature;

use super::jsonc_edit::{self, FeatureEdit};
use super::prompts;
use super::resolve::{self, CommonFeatureFlags};

const DONE: &str = "Done";
const ADD_FEATURE: &str = "Add a feature";
const REMOVE_FEATURE: &str = "Remove a feature";
const EDIT_OPTIONS: &str = "Edit feature options";

/// Edit features in an existing devcontainer configuration.
#[derive(Args)]
pub struct EditArgs {
    #[command(flatten)]
    pub common: CommonFeatureFlags,

    /// Add a feature (non-interactive, repeatable).
    #[arg(long, value_name = "OCI_REF")]
    pub add: Vec<String>,

    /// Remove a feature (non-interactive, repeatable).
    #[arg(long, value_name = "REF")]
    pub remove: Vec<String>,

    /// Set a feature option: REF=KEY=VALUE (non-interactive, repeatable).
    #[arg(long = "set-option", value_name = "REF=KEY=VALUE")]
    pub set_option: Vec<String>,
}

impl EditArgs {
    const fn is_non_interactive(&self) -> bool {
        !self.add.is_empty() || !self.remove.is_empty() || !self.set_option.is_empty()
    }

    /// Execute the edit command.
    ///
    /// # Errors
    ///
    /// Returns error on config discovery failure, parse errors, or I/O errors.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        if self.is_non_interactive() {
            self.run_non_interactive()
        } else {
            self.run_interactive().await
        }
    }

    fn run_non_interactive(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = resolve::discover_config(&self.common)?;
        let raw = resolve::read_raw_config(&config_path)?;
        let stripped = cella_config::jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;
        let features = resolve::extract_features(&config);

        let mut edits: Vec<FeatureEdit> = Vec::new();

        // Process --add flags
        for reference in &self.add {
            edits.push(FeatureEdit::Add {
                reference: reference.clone(),
                options: serde_json::json!({}),
            });
        }

        // Process --remove flags
        for remove_ref in &self.remove {
            let full_ref = resolve::match_feature_ref(remove_ref, &features)
                .ok_or_else(|| {
                    let configured: Vec<&str> = features.iter().map(|(r, _)| r.as_str()).collect();
                    format!(
                        "feature '{remove_ref}' not found in config; configured features: {configured:?}"
                    )
                })?;
            edits.push(FeatureEdit::Remove {
                reference: full_ref.to_owned(),
            });
        }

        // Process --set-option flags
        for opt_str in &self.set_option {
            let (ref_id, key, value) = parse_set_option_flag(opt_str)?;
            let full_ref = resolve::match_feature_ref(&ref_id, &features).ok_or_else(|| {
                format!("feature '{ref_id}' not found in config for --set-option")
            })?;
            edits.push(FeatureEdit::SetOption {
                reference: full_ref.to_owned(),
                key,
                value,
            });
        }

        let result = jsonc_edit::apply_edits(&raw, &edits)?;
        std::fs::write(&config_path, result)?;
        eprintln!("\u{2713} Updated {}", config_path.display());

        Ok(())
    }

    async fn run_interactive(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = resolve::discover_config(&self.common)?;
        let raw = resolve::read_raw_config(&config_path)?;
        let stripped = cella_config::jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;
        let cache = TemplateCache::new();

        let mut current_features = resolve::extract_features(&config);
        let mut edits: Vec<FeatureEdit> = Vec::new();

        loop {
            display_current_features(&current_features, &cache).await;

            let actions = vec![
                DONE.to_owned(),
                ADD_FEATURE.to_owned(),
                REMOVE_FEATURE.to_owned(),
                EDIT_OPTIONS.to_owned(),
            ];
            let selection = Select::new("Action:", actions).prompt()?;

            match selection.as_str() {
                DONE => break,
                ADD_FEATURE => {
                    handle_add_feature(
                        &mut current_features,
                        &mut edits,
                        &cache,
                        self.common.registry.as_deref(),
                    )
                    .await?;
                }
                REMOVE_FEATURE => {
                    handle_remove_feature(&mut current_features, &mut edits)?;
                }
                EDIT_OPTIONS => {
                    handle_edit_options(&mut current_features, &mut edits, &cache).await?;
                }
                _ => unreachable!(),
            }
        }

        if edits.is_empty() {
            eprintln!("No changes made.");
            return Ok(());
        }

        let result = jsonc_edit::apply_edits(&raw, &edits)?;
        std::fs::write(&config_path, result)?;
        eprintln!("\u{2713} Updated {}", config_path.display());

        Ok(())
    }
}

/// Display the list of currently configured features.
async fn display_current_features(features: &[(String, serde_json::Value)], cache: &TemplateCache) {
    if features.is_empty() {
        eprintln!("\nNo features configured.");
    } else {
        eprintln!("\nCurrent features:");
        for (i, (reference, options)) in features.iter().enumerate() {
            let name = resolve::resolve_feature_name(reference, cache).await;
            let opts = format_opts(options);
            if opts.is_empty() {
                eprintln!("  {}. {} ({})", i + 1, name, reference);
            } else {
                eprintln!("  {}. {} ({})\n        {}", i + 1, name, reference, opts);
            }
        }
    }
}

/// Convert a `HashMap` of options into a `serde_json::Value::Object`.
fn options_to_json(opts: &HashMap<String, serde_json::Value>) -> serde_json::Value {
    if opts.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::Value::Object(opts.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
}

/// Handle the "Add a feature" interactive action.
async fn handle_add_feature(
    current_features: &mut Vec<(String, serde_json::Value)>,
    edits: &mut Vec<FeatureEdit>,
    cache: &TemplateCache,
    registry: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some((feature, opts)) = prompt_add_feature(cache, registry).await? {
        current_features.push((feature.reference.clone(), options_to_json(&feature.options)));
        edits.push(FeatureEdit::Add {
            reference: feature.reference,
            options: options_to_json(&opts),
        });
    }
    Ok(())
}

/// Handle the "Remove a feature" interactive action.
fn handle_remove_feature(
    current_features: &mut Vec<(String, serde_json::Value)>,
    edits: &mut Vec<FeatureEdit>,
) -> Result<(), Box<dyn std::error::Error>> {
    if current_features.is_empty() {
        eprintln!("No features to remove.");
        return Ok(());
    }
    let choices: Vec<String> = current_features.iter().map(|(r, _)| r.clone()).collect();
    let selected = Select::new("Remove which feature?", choices).prompt()?;
    current_features.retain(|(r, _)| r != &selected);
    edits.push(FeatureEdit::Remove {
        reference: selected,
    });
    Ok(())
}

/// Handle the "Edit feature options" interactive action.
async fn handle_edit_options(
    current_features: &mut [(String, serde_json::Value)],
    edits: &mut Vec<FeatureEdit>,
    cache: &TemplateCache,
) -> Result<(), Box<dyn std::error::Error>> {
    if current_features.is_empty() {
        eprintln!("No features to edit.");
        return Ok(());
    }
    let choices: Vec<String> = current_features.iter().map(|(r, _)| r.clone()).collect();
    let selected = Select::new("Edit options for which feature?", choices).prompt()?;

    let current_opts = current_features
        .iter()
        .find(|(r, _)| r == &selected)
        .map_or_else(|| serde_json::json!({}), |(_, o)| o.clone());

    let new_opts = prompt_edit_options(&selected, &current_opts, cache).await?;

    if let Some(entry) = current_features.iter_mut().find(|(r, _)| r == &selected) {
        entry.1 = options_to_json(&new_opts);
    }

    edits.push(FeatureEdit::ReplaceOptions {
        reference: selected,
        options: serde_json::Value::Object(new_opts.into_iter().collect()),
    });
    Ok(())
}

/// Prompt user to select and configure a new feature.
async fn prompt_add_feature(
    cache: &TemplateCache,
    registry: Option<&str>,
) -> Result<Option<(SelectedFeature, HashMap<String, serde_json::Value>)>, Box<dyn std::error::Error>>
{
    let reg = registry.unwrap_or(DEFAULT_FEATURE_COLLECTION);
    let collection = collection::fetch_feature_collection(reg, cache, false).await?;

    if collection.features.is_empty() {
        eprintln!("No features found in {reg}.");
        return Ok(None);
    }

    let mut choices: Vec<String> = collection
        .features
        .iter()
        .map(|f| {
            let name = f.name.as_deref().unwrap_or(&f.id);
            let desc = f
                .description
                .as_deref()
                .map(|d| format!(" - {d}"))
                .unwrap_or_default();
            format!("{name}{desc}")
        })
        .collect();
    choices.insert(0, "(cancel)".to_owned());

    let selection = Select::new("Select a feature to add:", choices)
        .with_page_size(15)
        .prompt()?;

    if selection == "(cancel)" {
        return Ok(None);
    }

    // Find selected feature
    let idx = collection
        .features
        .iter()
        .position(|f| {
            let name = f.name.as_deref().unwrap_or(&f.id);
            selection.starts_with(name)
        })
        .ok_or("feature not found in selection")?;

    let feature_summary = &collection.features[idx];
    let feature_ref = format!("{reg}/{}:{}", feature_summary.id, feature_summary.version);

    // Fetch and prompt for options
    let feature_options =
        if let Ok(feature_dir) = fetcher::fetch_template(&feature_ref, cache).await {
            std::fs::read_to_string(feature_dir.join("devcontainer-feature.json"))
                .ok()
                .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
                .map(|meta| prompts::prompt_feature_options(&feature_summary.id, &meta))
                .transpose()?
                .unwrap_or_default()
        } else {
            eprintln!("  (could not fetch feature metadata; using defaults)");
            HashMap::new()
        };

    let name = feature_summary
        .name
        .as_deref()
        .unwrap_or(&feature_summary.id);
    eprintln!("\u{2713} Added: {name}");

    let selected = SelectedFeature {
        reference: feature_ref,
        options: feature_options.clone(),
    };

    Ok(Some((selected, feature_options)))
}

/// Prompt to edit options for an existing feature, using current values as defaults.
async fn prompt_edit_options(
    reference: &str,
    current_options: &serde_json::Value,
    cache: &TemplateCache,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error>> {
    // Try to fetch metadata for proper option types
    if let Ok(feature_dir) = fetcher::fetch_template(reference, cache).await
        && let Ok(content) = std::fs::read_to_string(feature_dir.join("devcontainer-feature.json"))
        && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(options_obj) = meta.get("options").and_then(|o| o.as_object())
    {
        let mut resolved = HashMap::new();
        for (key, opt_value) in options_obj {
            let mut opt: cella_templates::types::TemplateOption =
                match serde_json::from_value(opt_value.clone()) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
            // Override default with current value if present
            if let Some(current) = current_options.get(key) {
                opt.default = current.clone();
            }
            let value = prompts::prompt_single_option(key, &opt)?;
            resolved.insert(key.clone(), value);
        }
        return Ok(resolved);
    }

    // Fallback: free-form text editing of current options
    eprintln!("  (could not fetch feature metadata; editing as free-form text)");
    let mut resolved = HashMap::new();
    if let Some(obj) = current_options.as_object() {
        for (key, value) in obj {
            let default = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let new_val = inquire::Text::new(&format!("{key}:"))
                .with_default(&default)
                .prompt()?;
            resolved.insert(key.clone(), serde_json::json!(new_val));
        }
    }
    Ok(resolved)
}

/// Parse a `--set-option` flag value: `REF=KEY=VALUE`.
fn parse_set_option_flag(
    s: &str,
) -> Result<(String, String, serde_json::Value), Box<dyn std::error::Error>> {
    let (ref_id, rest) = s
        .split_once('=')
        .ok_or_else(|| format!("invalid --set-option format: {s:?} (expected REF=KEY=VALUE)"))?;
    let (key, value) = rest
        .split_once('=')
        .ok_or_else(|| format!("invalid --set-option format: {s:?} (expected REF=KEY=VALUE)"))?;
    Ok((
        ref_id.to_owned(),
        key.to_owned(),
        serde_json::Value::String(value.to_owned()),
    ))
}

/// Format options as a display string.
fn format_opts(options: &serde_json::Value) -> String {
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_set_option_simple() {
        let (ref_id, key, value) = parse_set_option_flag("node=version=lts").unwrap();
        assert_eq!(ref_id, "node");
        assert_eq!(key, "version");
        assert_eq!(value, serde_json::json!("lts"));
    }

    #[test]
    fn parse_set_option_full_ref() {
        let (ref_id, key, value) =
            parse_set_option_flag("ghcr.io/devcontainers/features/node:1=version=20").unwrap();
        assert_eq!(ref_id, "ghcr.io/devcontainers/features/node:1");
        assert_eq!(key, "version");
        assert_eq!(value, serde_json::json!("20"));
    }

    #[test]
    fn parse_set_option_invalid_no_equals() {
        assert!(parse_set_option_flag("noequalssign").is_err());
    }

    #[test]
    fn parse_set_option_invalid_one_equals() {
        assert!(parse_set_option_flag("node=version").is_err());
    }
}
