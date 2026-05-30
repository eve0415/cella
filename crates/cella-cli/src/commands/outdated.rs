//! `cella outdated` — show current and available versions for configured features.

use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use crate::commands::OutputFormat;
use crate::commands::features::resolve::{self, CommonFeatureFlags};
use crate::commands::features::update;
use crate::table::{Column, Table};

/// Show current and available versions.
#[derive(Args)]
pub struct OutdatedArgs {
    #[command(flatten)]
    pub common: CommonFeatureFlags,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,

    /// Exit with code 1 if any feature is outdated.
    #[arg(long)]
    pub check: bool,

    #[arg(long, hide = true)]
    pub terminal_columns: Option<u16>,

    #[arg(long, hide = true)]
    pub terminal_rows: Option<u16>,

    #[arg(long, hide = true)]
    pub user_data_folder: Option<PathBuf>,

    #[arg(long, hide = true)]
    pub log_level: Option<String>,

    #[arg(long, hide = true)]
    pub log_format: Option<String>,
}

impl OutdatedArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config_path = resolve::discover_config(&self.common)?;
        let raw = resolve::read_raw_config(&config_path)?;
        let stripped = cella_jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;
        let features = resolve::extract_features(&config);

        if features.is_empty() {
            if matches!(self.output, OutputFormat::Json) {
                println!("{{\"features\":{{}}}}");
            } else {
                eprintln!("No features configured.");
            }
            return Ok(());
        }

        let registry = self
            .common
            .registry
            .as_deref()
            .unwrap_or(cella_templates::collection::DEFAULT_FEATURE_COLLECTION);
        let cache = cella_templates::cache::TemplateCache::new();
        let collection =
            cella_templates::collection::fetch_feature_collection(registry, &cache, false).await?;

        let mut entries: Vec<FeatureStatus> = Vec::new();
        for (reference, _) in &features {
            if let Some(candidate) = update::check_for_update(reference, &collection, &cache).await
            {
                entries.push(FeatureStatus {
                    name: candidate.name.clone(),
                    reference: reference.clone(),
                    current: candidate.current_tag.clone(),
                    latest: candidate.latest_version.clone(),
                    outdated: true,
                });
            } else {
                let current_tag = reference.rsplit_once(':').map_or("latest", |(_, tag)| tag);
                let name = resolve::resolve_feature_name(reference, &cache).await;
                entries.push(FeatureStatus {
                    name,
                    reference: reference.clone(),
                    current: current_tag.to_owned(),
                    latest: current_tag.to_owned(),
                    outdated: false,
                });
            }
        }

        match self.output {
            OutputFormat::Auto | OutputFormat::Text => print_table(&entries),
            OutputFormat::Json => print_json(&entries)?,
        }

        if self.check && entries.iter().any(|e| e.outdated) {
            std::process::exit(1);
        }

        Ok(())
    }
}

struct FeatureStatus {
    name: String,
    reference: String,
    current: String,
    latest: String,
    outdated: bool,
}

fn print_table(entries: &[FeatureStatus]) {
    if entries.is_empty() {
        eprintln!("All features are up to date.");
        return;
    }

    let mut table = Table::new(vec![
        Column::shrinkable("FEATURE"),
        Column::fixed("CURRENT"),
        Column::fixed("LATEST"),
        Column::fixed("STATUS"),
    ]);

    for entry in entries {
        let status = if entry.outdated {
            "outdated"
        } else {
            "up to date"
        };
        table.add_row(vec![
            entry.name.clone(),
            entry.current.clone(),
            entry.latest.clone(),
            status.to_owned(),
        ]);
    }

    table.eprint();
}

fn print_json(entries: &[FeatureStatus]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut features = serde_json::Map::new();
    for entry in entries {
        features.insert(
            entry.reference.clone(),
            json!({
                "current": entry.current,
                "latest": entry.latest,
                "updateAvailable": entry.outdated,
            }),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ "features": features }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use cella_templates::cache::TemplateCache;
    use cella_templates::types::{FeatureCollectionIndex, FeatureSummary};

    use super::*;

    fn make_collection(features: Vec<FeatureSummary>) -> FeatureCollectionIndex {
        FeatureCollectionIndex {
            features,
            source_information: None,
        }
    }

    #[test]
    fn outdated_entry_from_update_candidate() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = TemplateCache::with_root(std::env::temp_dir().join("cella-test-outdated-1"));
        let collection = make_collection(vec![FeatureSummary {
            id: "node".to_owned(),
            version: "2.0.0".to_owned(),
            name: Some("Node.js".to_owned()),
            description: None,
            keywords: vec![],
        }]);

        let candidate = rt.block_on(update::check_for_update(
            "ghcr.io/devcontainers/features/node:1",
            &collection,
            &cache,
        ));
        assert!(candidate.is_some());
        let c = candidate.unwrap();
        assert_eq!(c.current_tag, "1");
        assert_eq!(c.latest_version, "2.0.0");
    }

    #[test]
    fn outdated_entry_up_to_date_returns_none() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = TemplateCache::with_root(std::env::temp_dir().join("cella-test-outdated-2"));
        let collection = make_collection(vec![FeatureSummary {
            id: "node".to_owned(),
            version: "1".to_owned(),
            name: Some("Node.js".to_owned()),
            description: None,
            keywords: vec![],
        }]);

        let candidate = rt.block_on(update::check_for_update(
            "ghcr.io/devcontainers/features/node:1",
            &collection,
            &cache,
        ));
        assert!(candidate.is_none());
    }

    #[test]
    fn print_table_empty_shows_up_to_date() {
        print_table(&[]);
    }

    #[test]
    fn print_json_empty_produces_valid_json() {
        let entries: Vec<FeatureStatus> = vec![];
        let result = print_json(&entries);
        assert!(result.is_ok());
    }

    #[test]
    fn print_json_with_entries_produces_valid_structure() {
        let entries = vec![FeatureStatus {
            name: "Node.js".to_owned(),
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            current: "1".to_owned(),
            latest: "2.0.0".to_owned(),
            outdated: true,
        }];
        let result = print_json(&entries);
        assert!(result.is_ok());
    }

    #[test]
    fn print_table_with_entries_does_not_panic() {
        let entries = vec![FeatureStatus {
            name: "Node.js".to_owned(),
            reference: "ghcr.io/devcontainers/features/node:1".to_owned(),
            current: "1".to_owned(),
            latest: "2.0.0".to_owned(),
            outdated: true,
        }];
        print_table(&entries);
    }
}
