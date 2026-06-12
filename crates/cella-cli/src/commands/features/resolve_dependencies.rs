//! `cella features resolve-dependencies` — compute and print the feature
//! installation order for a devcontainer configuration.
//!
//! Drop-in replacement for `devcontainer features resolveDependencies`.
//!
//! The official CLI (`featuresResolveDependenciesOptions`) exposes only
//! `--workspace-folder` and `--log-level`; there is no `--config` flag.
//! Config discovery is auto-detection from the workspace folder, matching the
//! official behaviour exactly.
//!
//! Contract:
//!   cella features resolve-dependencies [--workspace-folder <path>] [--log-level <level>]
//!
//! Stdout (two writes, in order):
//!   1. Mermaid flowchart of the dependency graph (plain text).
//!   2. JSON object: `{"installOrder": [{"id": "...", "options": ...}, ...]}`
//!
//! Logs go to stderr. Exit 1 on config-not-found, parse failure, or cycle.
//!
//! Cycle handling: the official CLI returns `undefined` from
//! `computeDependsOnInstallationOrder` and calls `process.exit(1)` — no JSON
//! output, bold error to stderr. We match that exactly.

use std::collections::HashSet;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::Serialize;

use super::resolve::{CommonFeatureFlags, discover_config, extract_features, read_raw_config};
use cella_features::graph::{
    DepEdge, DependencyGraph, EdgeKind, build_dependency_graph, render_mermaid,
};
use cella_features::ordering::compute_install_order;
use cella_features::{FeatureMetadata, FeatureWarning};

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// Resolve and print the installation order for features in a devcontainer
/// configuration.
///
/// The official `devcontainer features resolveDependencies` command only
/// exposes `--workspace-folder` and `--log-level`. There is no `--config` flag
/// for this subcommand; config discovery is automatic from the workspace folder.
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
            // No --config flag for this subcommand (matches official CLI).
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

        // Single OCI fetch pass feeds both Mermaid output AND install-order
        // metadata. Falls back gracefully on fetch errors (warn + declared
        // features only, no transitive deps).
        let root_refs: Vec<&str> = feature_pairs
            .iter()
            .map(|(r, _)| r.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let graph_result = build_dependency_graph(&root_refs).await;

        let (mermaid, ordered_ids) = match graph_result {
            Ok(graph) => {
                let mermaid = build_mermaid_from_graph(&feature_pairs, &graph);
                let order_result =
                    compute_order_with_metadata(&feature_pairs, &graph, override_order.as_deref());
                match order_result {
                    Ok(ids) => (mermaid, ids),
                    Err(cycle_msg) => {
                        // Print the Mermaid diagram first (we have it), then
                        // exit non-zero without JSON output — matches official.
                        println!("{mermaid}");
                        eprintln!("\u{001b}[1mNo viable installation order!\u{001b}[22m");
                        eprintln!("{cycle_msg}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: OCI fetch failed, falling back to declared order: {e}");
                let mermaid = build_mermaid_declared_only(&feature_pairs);
                let ids = compute_order_declared_only(&feature_pairs, override_order.as_deref())?;
                (mermaid, ids)
            }
        };

        println!("{mermaid}");

        let install_order = build_install_order_entries(&ordered_ids, &feature_pairs);
        println!(
            "{}",
            serde_json::to_string_pretty(&InstallOrderOutput { install_order })?
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Graph → Mermaid helpers
// ---------------------------------------------------------------------------

/// Build the Mermaid diagram from a successfully fetched `DependencyGraph`.
///
/// Deduplicates edges and renders all declared roots as starting points,
/// matching the official CLI's `generateMermaidDiagram` which iterates over
/// all `user-provided` (root) nodes.
fn build_mermaid_from_graph(
    feature_pairs: &[(String, serde_json::Value)],
    graph: &DependencyGraph,
) -> String {
    let roots: Vec<&str> = feature_pairs.iter().map(|(r, _)| r.as_str()).collect();

    // Deduplicate by (from, to, kind-discriminant).
    let deduped = dedup_edges(&graph.edges);
    render_mermaid(&roots, &deduped)
}

/// Build a fallback Mermaid diagram when OCI fetch failed.
///
/// Renders all declared roots as isolated nodes (no edge data available).
fn build_mermaid_declared_only(feature_pairs: &[(String, serde_json::Value)]) -> String {
    let roots: Vec<&str> = feature_pairs.iter().map(|(r, _)| r.as_str()).collect();
    render_mermaid(&roots, &[])
}

/// Deduplicate graph edges by `(from, to, kind)`.
fn dedup_edges(edges: &[DepEdge]) -> Vec<DepEdge> {
    let mut seen: HashSet<(String, String, u8)> = HashSet::new();
    let mut result: Vec<DepEdge> = Vec::new();
    for edge in edges {
        let (from, to, kind) = edge;
        let kind_tag = u8::from(matches!(kind, EdgeKind::InstallsAfter));
        if seen.insert((from.clone(), to.clone(), kind_tag)) {
            result.push(edge.clone());
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Install-order computation
// ---------------------------------------------------------------------------

/// Compute install order using real OCI-fetched metadata.
///
/// Expands the feature set with transitive `dependsOn` nodes, then runs the
/// topological sort with real `installs_after` data.
///
/// Returns `Err(message)` when a cycle is detected — caller must treat this as
/// fatal (exit non-zero, no JSON).
fn compute_order_with_metadata(
    feature_pairs: &[(String, serde_json::Value)],
    graph: &DependencyGraph,
    override_order: Option<&[String]>,
) -> Result<Vec<String>, String> {
    // Build the expanded feature set: declared roots + all transitive deps.
    // Preserve declaration order for roots, append transitives at the end.
    let declared_ids: Vec<&str> = feature_pairs.iter().map(|(r, _)| r.as_str()).collect();
    let mut all_ids: Vec<String> = declared_ids.iter().map(|s| (*s).to_owned()).collect();

    for fetched_id in graph.metadata.keys() {
        if !declared_ids.contains(&fetched_id.as_str()) {
            all_ids.push(fetched_id.clone());
        }
    }

    // Build (id, metadata) pairs using fetched data where available.
    let metas: Vec<(String, FeatureMetadata)> = all_ids
        .iter()
        .map(|id| {
            let meta = graph
                .metadata
                .get(id)
                .cloned()
                .unwrap_or_else(|| FeatureMetadata {
                    id: id.clone(),
                    ..Default::default()
                });
            (id.clone(), meta)
        })
        .collect();

    let order_input: Vec<(String, &FeatureMetadata)> =
        metas.iter().map(|(id, m)| (id.clone(), m)).collect();

    let (ordered_ids, warnings) = compute_install_order(&order_input, override_order);

    for w in &warnings {
        if let FeatureWarning::CyclicDependency { features } = w {
            return Err(format!(
                "cyclic dependency detected among features: {}",
                features.join(", ")
            ));
        }
    }

    Ok(ordered_ids)
}

/// Compute install order from declared features only (OCI fetch fallback).
///
/// Uses stub metadata (no `installs_after` data), so ordering is declaration
/// order + override list only.
///
/// Returns `Err(message)` on cycle (fatal).
fn compute_order_declared_only(
    feature_pairs: &[(String, serde_json::Value)],
    override_order: Option<&[String]>,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
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

    let (ordered_ids, warnings) = compute_install_order(&order_input, override_order);

    for w in &warnings {
        if let FeatureWarning::CyclicDependency { features } = w {
            return Err(format!(
                "cyclic dependency detected among features: {}",
                features.join(", ")
            )
            .into());
        }
    }

    Ok(ordered_ids)
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
    use std::collections::HashMap;

    use super::*;
    use cella_features::FeatureMetadata;

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
    // compute_order_declared_only
    // -----------------------------------------------------------------------

    #[test]
    fn order_no_deps_preserves_declaration_order() {
        let pairs = vec![
            ("ghcr.io/x/features/a:1".to_owned(), serde_json::json!({})),
            ("ghcr.io/x/features/b:1".to_owned(), serde_json::json!({})),
            ("ghcr.io/x/features/c:1".to_owned(), serde_json::json!({})),
        ];

        let order = compute_order_declared_only(&pairs, None).unwrap();

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

        let order = compute_order_declared_only(&pairs, Some(&overrides)).unwrap();

        assert_eq!(order[0], "ghcr.io/x/features/c:1");
        assert_eq!(order[1], "ghcr.io/x/features/a:1");
        assert_eq!(order[2], "ghcr.io/x/features/b:1");
    }

    #[test]
    fn order_empty_features() {
        let pairs: Vec<(String, serde_json::Value)> = vec![];
        let order = compute_order_declared_only(&pairs, None).unwrap();
        assert!(order.is_empty());
    }

    // -----------------------------------------------------------------------
    // cycle detection is fatal
    // -----------------------------------------------------------------------

    #[test]
    fn cycle_in_declared_only_is_fatal() {
        // installs_after cycles aren't possible with stub metadata (no
        // installs_after set), but test the error path directly via
        // compute_order_with_metadata using synthetic metadata with a cycle.
        let pairs = vec![
            ("feat-a".to_owned(), serde_json::json!({})),
            ("feat-b".to_owned(), serde_json::json!({})),
        ];

        // Build a graph with synthetic cyclic metadata.
        let mut meta_map = HashMap::new();
        meta_map.insert(
            "feat-a".to_owned(),
            FeatureMetadata {
                id: "feat-a".to_owned(),
                installs_after: vec!["feat-b".to_owned()],
                ..Default::default()
            },
        );
        meta_map.insert(
            "feat-b".to_owned(),
            FeatureMetadata {
                id: "feat-b".to_owned(),
                installs_after: vec!["feat-a".to_owned()],
                ..Default::default()
            },
        );

        let graph = DependencyGraph {
            edges: vec![],
            metadata: meta_map,
        };

        let result = compute_order_with_metadata(&pairs, &graph, None);
        assert!(
            result.is_err(),
            "cycle must be a fatal error, not a warning+fallback"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("cyclic"),
            "error message should mention cycle: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // transitive deps appear in installOrder
    // -----------------------------------------------------------------------

    #[test]
    fn transitive_dep_appears_in_install_order() {
        // Scenario: config declares only `node`. `node` dependsOn `common-utils`.
        // After OCI fetch, `common-utils` must appear in installOrder.
        let pairs = vec![(
            "ghcr.io/x/features/node:1".to_owned(),
            serde_json::json!({"version": "lts"}),
        )];

        let mut meta_map = HashMap::new();
        // node's metadata lists common-utils as a hard dep via installs_after
        // (the ordering algorithm uses installs_after; dependsOn is recorded
        // as a graph edge but the ordering crate uses installs_after for topo sort).
        meta_map.insert(
            "ghcr.io/x/features/node:1".to_owned(),
            FeatureMetadata {
                id: "ghcr.io/x/features/node:1".to_owned(),
                installs_after: vec!["ghcr.io/x/features/common-utils:2".to_owned()],
                ..Default::default()
            },
        );
        meta_map.insert(
            "ghcr.io/x/features/common-utils:2".to_owned(),
            FeatureMetadata {
                id: "ghcr.io/x/features/common-utils:2".to_owned(),
                ..Default::default()
            },
        );

        let graph = DependencyGraph {
            edges: vec![(
                "ghcr.io/x/features/node:1".to_owned(),
                "ghcr.io/x/features/common-utils:2".to_owned(),
                EdgeKind::DependsOn,
            )],
            metadata: meta_map,
        };

        let order = compute_order_with_metadata(&pairs, &graph, None).unwrap();

        assert!(
            order.contains(&"ghcr.io/x/features/common-utils:2".to_owned()),
            "transitive dep common-utils must appear in installOrder; got: {order:?}"
        );
        // common-utils must come before node (it's a prerequisite).
        let pos_common = order
            .iter()
            .position(|x| x == "ghcr.io/x/features/common-utils:2")
            .unwrap();
        let pos_node = order
            .iter()
            .position(|x| x == "ghcr.io/x/features/node:1")
            .unwrap();
        assert!(
            pos_common < pos_node,
            "common-utils must be installed before node; order: {order:?}"
        );
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

    // -----------------------------------------------------------------------
    // dedup_edges
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_edges_removes_duplicates() {
        let edges = vec![
            ("a".to_owned(), "b".to_owned(), EdgeKind::DependsOn),
            ("a".to_owned(), "b".to_owned(), EdgeKind::DependsOn),
            ("a".to_owned(), "c".to_owned(), EdgeKind::InstallsAfter),
        ];
        let deduped = dedup_edges(&edges);
        assert_eq!(deduped.len(), 2);
    }
}
