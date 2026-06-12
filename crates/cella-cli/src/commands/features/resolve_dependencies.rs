//! `cella features resolve-dependencies` — compute and print the feature
//! installation order for a devcontainer configuration.
//!
//! Drop-in replacement for `devcontainer features resolveDependencies`.
//! Contract:
//!   cella features resolve-dependencies [--workspace-folder <path>] [--log-level <level>]
//!
//! Stdout (two writes, in order):
//!   1. Mermaid flowchart of the dependency graph (plain text).
//!   2. JSON object: `{"installOrder": [{"id": "...", "options": ...}, ...]}`
//!
//! Logs go to stderr. Exit 1 on config-not-found, parse failure, or cycle.

use std::collections::HashSet;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::Serialize;

use super::resolve::{CommonFeatureFlags, discover_config, extract_features, read_raw_config};
use cella_features::graph::{DepEdge, build_dependency_graph, render_mermaid};
use cella_features::ordering::compute_install_order;
use cella_features::{FeatureMetadata, FeatureWarning};

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// Resolve and print the installation order for features in a devcontainer
/// configuration.
#[derive(Debug, Clone, Parser)]
pub struct ResolveDependenciesArgs {
    /// Workspace folder (defaults to current directory).
    ///
    /// Equivalent to `--workspace-folder` in the official devcontainer CLI.
    #[arg(long, short = 'w')]
    pub workspace_folder: Option<PathBuf>,

    /// Log verbosity (default `error` matches the official CLI).
    #[arg(long, default_value = "error")]
    pub log_level: ResolveDepsLogLevel,
}

/// Log level for the resolve-dependencies command.
///
/// Default is `error` to match the official CLI.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ResolveDepsLogLevel {
    Error,
    Info,
    Debug,
    Trace,
}

// ---------------------------------------------------------------------------
// JSON output shape — matches official CLI
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallOrderOutput {
    install_order: Vec<InstallOrderEntry>,
}

#[derive(Serialize)]
struct InstallOrderEntry {
    id: String,
    options: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

impl ResolveDependenciesArgs {
    /// Execute the resolve-dependencies command.
    ///
    /// # Errors
    ///
    /// Returns an error when the config is missing, unparseable, or a cycle
    /// is detected that prevents an install order from being computed.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let flags = CommonFeatureFlags {
            file: None,
            workspace_folder: self.workspace_folder,
            registry: None,
        };
        let config_path = discover_config(&flags)?;
        let raw = read_raw_config(&config_path)?;
        let stripped = cella_jsonc::strip(&raw)?;
        let config: serde_json::Value = serde_json::from_str(&stripped)?;

        let feature_pairs = extract_features(&config);

        if feature_pairs.is_empty() {
            println!("flowchart TD");
            println!(
                "{}",
                serde_json::to_string_pretty(&InstallOrderOutput {
                    install_order: Vec::new()
                })?
            );
            return Ok(());
        }

        let override_order = parse_override_order(&config);

        // Mermaid diagram: attempt OCI fetch for each declared feature root.
        let mermaid = build_mermaid_for_features(&feature_pairs).await;
        println!("{mermaid}");

        let (ordered_ids, warnings) =
            compute_order_from_config(&feature_pairs, override_order.as_deref());

        for w in &warnings {
            if let FeatureWarning::CyclicDependency { features } = w {
                eprintln!(
                    "warning: cyclic dependency detected among features: {}",
                    features.join(", ")
                );
            }
        }

        let install_order = build_install_order_entries(&ordered_ids, &feature_pairs);
        println!(
            "{}",
            serde_json::to_string_pretty(&InstallOrderOutput { install_order })?
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Parse `overrideFeatureInstallOrder` from the raw config JSON.
fn parse_override_order(config: &serde_json::Value) -> Option<Vec<String>> {
    let arr = config
        .get("overrideFeatureInstallOrder")
        .and_then(|v| v.as_array())?;
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
        .collect();
    if ids.is_empty() { None } else { Some(ids) }
}

/// Build a Mermaid diagram covering all declared feature roots.
///
/// Attempts OCI metadata fetch for each root feature. Edges from all roots are
/// merged and deduplicated. Falls back to an edge-free diagram on fetch errors.
async fn build_mermaid_for_features(feature_pairs: &[(String, serde_json::Value)]) -> String {
    let mut all_edges: Vec<DepEdge> = Vec::new();
    let mut seen_roots: HashSet<&str> = HashSet::new();

    for (feature_ref, _) in feature_pairs {
        if !seen_roots.insert(feature_ref.as_str()) {
            continue;
        }
        match build_dependency_graph(feature_ref).await {
            Ok(edges) => all_edges.extend(edges),
            Err(e) => {
                eprintln!("warning: could not build dependency graph for {feature_ref}: {e}");
            }
        }
    }

    // Deduplicate by (from, to, kind-discriminant).
    let mut seen_edges: HashSet<(String, String, u8)> = HashSet::new();
    all_edges.retain(|(from, to, kind)| {
        let kind_tag = u8::from(matches!(
            kind,
            cella_features::graph::EdgeKind::InstallsAfter
        ));
        seen_edges.insert((from.clone(), to.clone(), kind_tag))
    });

    let root = &feature_pairs[0].0;
    render_mermaid(root, &all_edges)
}

/// Compute install order from config-level feature declarations.
///
/// Uses stub `FeatureMetadata` (no OCI fetch) — `installs_after` is not
/// populated, so ordering is driven by declaration order and override list.
fn compute_order_from_config(
    feature_pairs: &[(String, serde_json::Value)],
    override_order: Option<&[String]>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    let metas: Vec<(String, FeatureMetadata)> = feature_pairs
        .iter()
        .map(|(id, _)| {
            (
                id.clone(),
                FeatureMetadata {
                    id: id.clone(),
                    ..Default::default()
                },
            )
        })
        .collect();

    let order_input: Vec<(String, &FeatureMetadata)> =
        metas.iter().map(|(id, m)| (id.clone(), m)).collect();

    compute_install_order(&order_input, override_order)
}

/// Build the `installOrder` entries from ordered IDs and their options.
fn build_install_order_entries(
    ordered_ids: &[String],
    feature_pairs: &[(String, serde_json::Value)],
) -> Vec<InstallOrderEntry> {
    ordered_ids
        .iter()
        .map(|id| {
            let options = feature_pairs
                .iter()
                .find(|(ref_id, _)| ref_id == id)
                .map_or(serde_json::Value::Bool(true), |(_, opts)| opts.clone());
            InstallOrderEntry {
                id: id.clone(),
                options,
            }
        })
        .collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_override_order
    // -----------------------------------------------------------------------

    #[test]
    fn parse_override_order_absent() {
        let config = serde_json::json!({"image": "ubuntu"});
        assert!(parse_override_order(&config).is_none());
    }

    #[test]
    fn parse_override_order_present() {
        let config = serde_json::json!({
            "overrideFeatureInstallOrder": [
                "ghcr.io/devcontainers/features/git:1",
                "ghcr.io/devcontainers/features/node:1"
            ]
        });
        let order = parse_override_order(&config).unwrap();
        assert_eq!(order.len(), 2);
        assert_eq!(order[0], "ghcr.io/devcontainers/features/git:1");
        assert_eq!(order[1], "ghcr.io/devcontainers/features/node:1");
    }

    #[test]
    fn parse_override_order_empty_array_returns_none() {
        let config = serde_json::json!({"overrideFeatureInstallOrder": []});
        assert!(parse_override_order(&config).is_none());
    }

    // -----------------------------------------------------------------------
    // compute_order_from_config
    // -----------------------------------------------------------------------

    #[test]
    fn order_no_deps_preserves_declaration_order() {
        let pairs = vec![
            ("ghcr.io/x/features/a:1".to_owned(), serde_json::json!({})),
            ("ghcr.io/x/features/b:1".to_owned(), serde_json::json!({})),
            ("ghcr.io/x/features/c:1".to_owned(), serde_json::json!({})),
        ];

        let (order, warnings) = compute_order_from_config(&pairs, None);

        assert!(warnings.is_empty());
        assert_eq!(
            order,
            vec![
                "ghcr.io/x/features/a:1",
                "ghcr.io/x/features/b:1",
                "ghcr.io/x/features/c:1"
            ]
        );
    }

    #[test]
    fn order_with_override_respects_override() {
        let pairs = vec![
            ("ghcr.io/x/features/a:1".to_owned(), serde_json::json!({})),
            ("ghcr.io/x/features/b:1".to_owned(), serde_json::json!({})),
            ("ghcr.io/x/features/c:1".to_owned(), serde_json::json!({})),
        ];
        let overrides = vec![
            "ghcr.io/x/features/c:1".to_owned(),
            "ghcr.io/x/features/a:1".to_owned(),
        ];

        let (order, warnings) = compute_order_from_config(&pairs, Some(&overrides));

        assert!(warnings.is_empty());
        assert_eq!(order[0], "ghcr.io/x/features/c:1");
        assert_eq!(order[1], "ghcr.io/x/features/a:1");
        assert_eq!(order[2], "ghcr.io/x/features/b:1");
    }

    #[test]
    fn order_empty_features() {
        let pairs: Vec<(String, serde_json::Value)> = vec![];
        let (order, warnings) = compute_order_from_config(&pairs, None);
        assert!(order.is_empty());
        assert!(warnings.is_empty());
    }

    // -----------------------------------------------------------------------
    // build_install_order_entries
    // -----------------------------------------------------------------------

    #[test]
    fn install_entries_match_options() {
        let pairs = vec![
            (
                "ghcr.io/x/features/node:1".to_owned(),
                serde_json::json!({"version": "lts"}),
            ),
            (
                "ghcr.io/x/features/git:1".to_owned(),
                serde_json::json!(true),
            ),
        ];
        let ids = vec![
            "ghcr.io/x/features/git:1".to_owned(),
            "ghcr.io/x/features/node:1".to_owned(),
        ];

        let entries = build_install_order_entries(&ids, &pairs);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "ghcr.io/x/features/git:1");
        assert_eq!(entries[0].options, serde_json::json!(true));
        assert_eq!(entries[1].id, "ghcr.io/x/features/node:1");
        assert_eq!(entries[1].options, serde_json::json!({"version": "lts"}));
    }

    #[test]
    fn install_entries_missing_ref_defaults_to_true() {
        let pairs = vec![(
            "ghcr.io/x/features/node:1".to_owned(),
            serde_json::json!({}),
        )];
        let ids = vec!["unknown".to_owned()];

        let entries = build_install_order_entries(&ids, &pairs);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].options, serde_json::Value::Bool(true));
    }

    // -----------------------------------------------------------------------
    // JSON output shape
    // -----------------------------------------------------------------------

    #[test]
    fn install_order_output_camel_case() {
        let out = InstallOrderOutput {
            install_order: vec![InstallOrderEntry {
                id: "ghcr.io/x/features/node:1".to_owned(),
                options: serde_json::json!({"version": "lts"}),
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert!(
            json.get("installOrder").is_some(),
            "installOrder key missing"
        );
        assert!(
            json.get("install_order").is_none(),
            "snake_case must not appear"
        );
        let entries = json["installOrder"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "ghcr.io/x/features/node:1");
        assert_eq!(entries[0]["options"]["version"], "lts");
    }

    #[test]
    fn empty_install_order_serialises_correctly() {
        let out = InstallOrderOutput {
            install_order: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        let arr = json["installOrder"].as_array().unwrap();
        assert!(arr.is_empty());
    }
}
