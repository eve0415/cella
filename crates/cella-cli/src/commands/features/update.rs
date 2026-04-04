//! `cella features update` — check for newer feature versions and apply updates.

use clap::Args;
use inquire::MultiSelect;

use cella_templates::cache::TemplateCache;

use super::jsonc_edit::{self, FeatureEdit};
use super::resolve::{self, CommonFeatureFlags};
use crate::commands::OutputFormat;

/// Check for and apply feature version updates.
#[derive(Args)]
pub struct UpdateArgs {
    #[command(flatten)]
    pub common: CommonFeatureFlags,

    /// Apply all available updates without prompting.
    #[arg(long)]
    pub yes: bool,

    /// Only check for updates, don't apply.
    #[arg(long)]
    pub check: bool,

    /// Output format (json implies --check).
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,
}

/// A feature with an available update.
#[derive(Debug, Clone)]
struct UpdateCandidate {
    /// Current full OCI reference (e.g. `ghcr.io/.../node:1`).
    current_ref: String,
    /// Display name of the feature.
    name: String,
    /// Current tag.
    current_tag: String,
    /// Latest available version from the collection.
    latest_version: String,
    /// Updated full reference.
    updated_ref: String,
}

impl UpdateArgs {
    /// Execute the update command.
    ///
    /// # Errors
    ///
    /// Returns error on config discovery failure or network errors.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = resolve::discover_config(&self.common)?;
        let raw = resolve::read_raw_config(&config_path)?;
        let stripped = cella_jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;
        let features = resolve::extract_features(&config);

        if features.is_empty() {
            print_empty_message(
                matches!(self.output, OutputFormat::Json),
                "No features configured.",
            );
            return Ok(());
        }

        let registry = self
            .common
            .registry
            .as_deref()
            .unwrap_or(cella_templates::collection::DEFAULT_FEATURE_COLLECTION);
        let cache = TemplateCache::new();
        let collection =
            cella_templates::collection::fetch_feature_collection(registry, &cache, false).await?;

        let candidates = find_update_candidates(&features, &collection, &cache).await;

        if candidates.is_empty() {
            print_empty_message(
                matches!(self.output, OutputFormat::Json),
                "All features are up to date.",
            );
            return Ok(());
        }

        if matches!(self.output, OutputFormat::Json) {
            print_candidates_json(&candidates)?;
            return Ok(());
        }

        display_update_table(&candidates);

        if self.check {
            return Ok(());
        }

        let to_update = select_updates(&candidates, self.yes)?;
        if to_update.is_empty() {
            eprintln!("No updates selected.");
            return Ok(());
        }

        apply_updates(&raw, &config_path, &features, &to_update)?;
        Ok(())
    }
}

/// Print an empty-list message as JSON or human-readable text.
fn print_empty_message(json: bool, text: &str) {
    if json {
        println!("[]");
    } else {
        eprintln!("{text}");
    }
}

/// Collect all features that have newer versions available.
async fn find_update_candidates(
    features: &[(String, serde_json::Value)],
    collection: &cella_templates::types::FeatureCollectionIndex,
    cache: &TemplateCache,
) -> Vec<UpdateCandidate> {
    let mut candidates = Vec::new();
    for (reference, _) in features {
        if let Some(candidate) = check_for_update(reference, collection, cache).await {
            candidates.push(candidate);
        }
    }
    candidates
}

/// Print update candidates as JSON.
fn print_candidates_json(candidates: &[UpdateCandidate]) -> Result<(), Box<dyn std::error::Error>> {
    let json_candidates: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "reference": c.current_ref,
                "name": c.name,
                "currentTag": c.current_tag,
                "latestVersion": c.latest_version,
                "updatedReference": c.updated_ref,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json_candidates)?);
    Ok(())
}

/// Display the update table to stderr.
fn display_update_table(candidates: &[UpdateCandidate]) {
    eprintln!("Available updates:");
    for candidate in candidates {
        eprintln!(
            "  {} ({}) : {} -> {}",
            candidate.name, candidate.current_ref, candidate.current_tag, candidate.latest_version
        );
    }
}

/// Prompt the user to select which updates to apply (or auto-select all with `--yes`).
fn select_updates(
    candidates: &[UpdateCandidate],
    auto_accept: bool,
) -> Result<Vec<UpdateCandidate>, Box<dyn std::error::Error>> {
    if auto_accept {
        return Ok(candidates.to_vec());
    }

    let choices: Vec<String> = candidates
        .iter()
        .map(|c| format!("{} : {} -> {}", c.name, c.current_tag, c.latest_version))
        .collect();
    let defaults: Vec<usize> = (0..choices.len()).collect();
    let selected = MultiSelect::new("Select features to update:", choices)
        .with_default(&defaults)
        .prompt()?;

    Ok(candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| selected.iter().any(|s| s.starts_with(&candidates[*i].name)))
        .map(|(_, c)| c.clone())
        .collect())
}

/// Build edits and write the updated config file.
fn apply_updates(
    raw: &str,
    config_path: &std::path::Path,
    features: &[(String, serde_json::Value)],
    to_update: &[UpdateCandidate],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut edits: Vec<FeatureEdit> = Vec::new();
    for candidate in to_update {
        let current_opts = features
            .iter()
            .find(|(r, _)| r == &candidate.current_ref)
            .map_or_else(|| serde_json::json!({}), |(_, o)| o.clone());

        edits.push(FeatureEdit::Remove {
            reference: candidate.current_ref.clone(),
        });
        edits.push(FeatureEdit::Add {
            reference: candidate.updated_ref.clone(),
            options: current_opts,
        });
    }

    let result = jsonc_edit::apply_edits(raw, &edits)?;
    std::fs::write(config_path, result)?;

    for candidate in to_update {
        eprintln!(
            "\u{2713} Updated {} : {} -> {}",
            candidate.name, candidate.current_tag, candidate.latest_version
        );
    }
    Ok(())
}

/// Check if a configured feature has an update available in the collection.
async fn check_for_update(
    reference: &str,
    collection: &cella_templates::types::FeatureCollectionIndex,
    cache: &TemplateCache,
) -> Option<UpdateCandidate> {
    // Parse the reference: registry/repo/id:tag
    let (base, current_tag) = reference.rsplit_once(':')?;
    let short_id = base.rsplit('/').next()?;

    // Find this feature in the collection
    let collection_entry = collection.features.iter().find(|f| f.id == short_id)?;

    // Compare versions: if collection version is different from current tag
    if collection_entry.version == current_tag {
        return None;
    }

    // Check if the collection version is actually newer
    // Simple heuristic: if they differ, the collection version is newer
    // (collection indexes always contain the latest version)
    let name = resolve::resolve_feature_name(reference, cache).await;

    let updated_ref = format!("{base}:{}", collection_entry.version);

    Some(UpdateCandidate {
        current_ref: reference.to_owned(),
        name,
        current_tag: current_tag.to_owned(),
        latest_version: collection_entry.version.clone(),
        updated_ref,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use cella_templates::types::{FeatureCollectionIndex, FeatureSummary};

    #[test]
    fn check_for_update_newer_version() {
        let collection = FeatureCollectionIndex {
            features: vec![FeatureSummary {
                id: "node".to_owned(),
                version: "2.0.0".to_owned(),
                name: Some("Node.js".to_owned()),
                description: None,
                keywords: vec![],
            }],
            source_information: None,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = cella_templates::cache::TemplateCache::with_root(
            std::env::temp_dir().join("cella-test-update"),
        );
        let result = rt.block_on(super::check_for_update(
            "ghcr.io/devcontainers/features/node:1",
            &collection,
            &cache,
        ));
        let candidate = result.unwrap();
        assert_eq!(candidate.current_tag, "1");
        assert_eq!(candidate.latest_version, "2.0.0");
        assert_eq!(
            candidate.updated_ref,
            "ghcr.io/devcontainers/features/node:2.0.0"
        );
    }

    #[test]
    fn check_for_update_same_version() {
        let collection = FeatureCollectionIndex {
            features: vec![FeatureSummary {
                id: "node".to_owned(),
                version: "1".to_owned(),
                name: Some("Node.js".to_owned()),
                description: None,
                keywords: vec![],
            }],
            source_information: None,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = cella_templates::cache::TemplateCache::with_root(
            std::env::temp_dir().join("cella-test-update2"),
        );
        let result = rt.block_on(super::check_for_update(
            "ghcr.io/devcontainers/features/node:1",
            &collection,
            &cache,
        ));
        assert!(result.is_none());
    }

    #[test]
    fn check_for_update_not_in_collection() {
        let collection = FeatureCollectionIndex {
            features: vec![],
            source_information: None,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = cella_templates::cache::TemplateCache::with_root(
            std::env::temp_dir().join("cella-test-update3"),
        );
        let result = rt.block_on(super::check_for_update(
            "ghcr.io/custom/features/myfeature:1",
            &collection,
            &cache,
        ));
        assert!(result.is_none());
    }

    #[test]
    fn check_for_update_no_tag_returns_none() {
        let collection = FeatureCollectionIndex {
            features: vec![FeatureSummary {
                id: "node".to_owned(),
                version: "2.0.0".to_owned(),
                name: None,
                description: None,
                keywords: vec![],
            }],
            source_information: None,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = cella_templates::cache::TemplateCache::with_root(
            std::env::temp_dir().join("cella-test-update4"),
        );
        // No colon → rsplit_once(':') returns None
        let result = rt.block_on(super::check_for_update(
            "ghcr.io/devcontainers/features/node",
            &collection,
            &cache,
        ));
        assert!(result.is_none());
    }

    #[test]
    fn print_candidates_json_valid_output() {
        let candidates = vec![super::UpdateCandidate {
            current_ref: "ghcr.io/devcontainers/features/node:1".to_owned(),
            name: "Node.js".to_owned(),
            current_tag: "1".to_owned(),
            latest_version: "2.0.0".to_owned(),
            updated_ref: "ghcr.io/devcontainers/features/node:2.0.0".to_owned(),
        }];
        // Just verify it doesn't error; stdout capture is awkward in tests
        assert!(super::print_candidates_json(&candidates).is_ok());
    }

    #[test]
    fn apply_updates_writes_file() {
        let dir = std::env::temp_dir().join("cella-test-apply-updates");
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("devcontainer.json");

        let raw = r#"{
  "features": {
    "ghcr.io/devcontainers/features/node:1": { "version": "lts" }
  }
}"#;
        std::fs::write(&config_path, raw).unwrap();

        let features = vec![(
            "ghcr.io/devcontainers/features/node:1".to_owned(),
            serde_json::json!({"version": "lts"}),
        )];

        let to_update = vec![super::UpdateCandidate {
            current_ref: "ghcr.io/devcontainers/features/node:1".to_owned(),
            name: "Node.js".to_owned(),
            current_tag: "1".to_owned(),
            latest_version: "2.0.0".to_owned(),
            updated_ref: "ghcr.io/devcontainers/features/node:2.0.0".to_owned(),
        }];

        super::apply_updates(raw, &config_path, &features, &to_update).unwrap();

        let result = std::fs::read_to_string(&config_path).unwrap();
        assert!(result.contains("node:2.0.0"));
        assert!(!result.contains("node:1\""));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
