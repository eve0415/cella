//! Non-interactive init mode: all parameters provided via CLI flags.

use std::collections::HashMap;

use cella_templates::cache::TemplateCache;
use cella_templates::fetcher;
use cella_templates::options;
use cella_templates::types::{OutputFormat, SelectedFeature};

use super::InitArgs;

/// Run `cella init` in non-interactive mode.
///
/// Requires `--template` to be set. All template/feature options are
/// parsed from CLI flags.
///
/// # Errors
///
/// Returns errors for invalid flags, network failures, or I/O errors.
pub async fn run(args: InitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let template_ref = args
        .template
        .as_deref()
        .expect("template required for non-interactive mode");
    let workspace = crate::commands::resolve_workspace_folder(args.workspace_folder.as_deref())?;
    let config_path = workspace.join(".devcontainer").join("devcontainer.json");

    // Check for existing config
    if config_path.exists() && !args.force {
        return Err(
            cella_templates::TemplateError::ConfigAlreadyExists { path: config_path }.into(),
        );
    }

    let cache = TemplateCache::new();

    // Fetch template artifact
    let template_dir = fetcher::fetch_template(template_ref, &cache).await?;
    let metadata = fetcher::read_template_metadata(&template_dir)?;

    // Parse template options from --template-option flags
    let user_template_opts = parse_key_value_pairs(&args.template_options)?;
    let resolved_opts =
        options::resolve_options(&metadata.id, &metadata.options, &user_template_opts)?;

    // Parse feature options from --option flags
    let features = parse_features(&args.feature, &args.option)?;

    // Apply template
    let written_path = cella_templates::apply::apply_template(
        &template_dir,
        &workspace,
        &resolved_opts,
        &features,
        OutputFormat::Jsonc,
    )?;

    eprintln!("\u{2713} Created {}", written_path.display());

    Ok(())
}

/// Parse `KEY=VALUE` pairs from CLI flag values.
fn parse_key_value_pairs(
    pairs: &[String],
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error>> {
    let mut map = HashMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("invalid option format: {pair:?} (expected KEY=VALUE)"))?;
        map.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
    }
    Ok(map)
}

/// Parse `--feature` and `--option` flags into `SelectedFeature` values.
///
/// `--option` format: `FEATURE_ID=KEY=VALUE`
fn parse_features(
    feature_refs: &[String],
    option_flags: &[String],
) -> Result<Vec<SelectedFeature>, Box<dyn std::error::Error>> {
    // Group options by feature ID
    let mut feature_options: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();
    for opt in option_flags {
        let (feature_id, rest) = opt.split_once('=').ok_or_else(|| {
            format!("invalid feature option format: {opt:?} (expected FEATURE_ID=KEY=VALUE)")
        })?;
        let (key, value) = rest.split_once('=').ok_or_else(|| {
            format!("invalid feature option format: {opt:?} (expected FEATURE_ID=KEY=VALUE)")
        })?;
        feature_options
            .entry(feature_id.to_owned())
            .or_default()
            .insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
    }

    let features = feature_refs
        .iter()
        .map(|reference| {
            // Try to extract a short ID for option matching
            let short_id = reference
                .rsplit('/')
                .next()
                .and_then(|s| s.split(':').next())
                .unwrap_or(reference);
            let opts = feature_options
                .remove(short_id)
                .or_else(|| feature_options.remove(reference))
                .unwrap_or_default();
            SelectedFeature {
                reference: reference.clone(),
                options: opts,
            }
        })
        .collect();

    Ok(features)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_value_simple() {
        let pairs = vec!["variant=bookworm".to_owned()];
        let map = parse_key_value_pairs(&pairs).unwrap();
        assert_eq!(map["variant"], serde_json::json!("bookworm"));
    }

    #[test]
    fn parse_key_value_multiple() {
        let pairs = vec!["variant=trixie".to_owned(), "debug=true".to_owned()];
        let map = parse_key_value_pairs(&pairs).unwrap();
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_key_value_invalid() {
        let pairs = vec!["noequalssign".to_owned()];
        assert!(parse_key_value_pairs(&pairs).is_err());
    }

    #[test]
    fn parse_features_empty() {
        let features = parse_features(&[], &[]).unwrap();
        assert!(features.is_empty());
    }

    #[test]
    fn parse_features_with_options() {
        let refs = vec!["ghcr.io/devcontainers/features/node:1".to_owned()];
        let opts = vec!["node=version=lts".to_owned()];
        let features = parse_features(&refs, &opts).unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].options["version"], serde_json::json!("lts"));
    }

    #[test]
    fn parse_features_no_options() {
        let refs = vec!["ghcr.io/devcontainers/features/node:1".to_owned()];
        let features = parse_features(&refs, &[]).unwrap();
        assert_eq!(features.len(), 1);
        assert!(features[0].options.is_empty());
    }
}
