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

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use super::resolve::{CommonFeatureFlags, discover_config, extract_features, read_raw_config};
use crate::commands::LogLevel;
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
    /// The official surface exposes the long flag only — no short alias.
    #[arg(long)]
    pub workspace_folder: Option<PathBuf>,

    /// Log verbosity (default `error` matches the official CLI).
    #[arg(long, default_value = "error")]
    pub log_level: LogLevel,
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
            // Bare `flowchart` keyword (no direction) matches the official CLI.
            println!("flowchart");
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
        //
        // Dedup while preserving first-seen declaration order — fetch and
        // traversal order must be deterministic so the resulting installOrder
        // is stable across runs.
        let root_refs = dedup_preserving_order(&feature_pairs);

        let graph_result = build_dependency_graph(&root_refs).await;

        let (mermaid, ordered_ids, transitive_options) = match graph_result {
            Ok(graph) => {
                let mermaid = build_mermaid_from_graph(&feature_pairs, &graph);
                let order_result =
                    compute_order_with_metadata(&feature_pairs, &graph, override_order.as_deref());
                match order_result {
                    Ok(ids) => (mermaid, ids, collect_transitive_options(&graph)),
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
                // No graph → no transitive deps, so no transitive options.
                (mermaid, ids, BTreeMap::new())
            }
        };

        println!("{mermaid}");

        let install_order =
            build_install_order_entries(&ordered_ids, &feature_pairs, &transitive_options);
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

    // `graph.metadata` is a `HashMap`, so iteration order is random. Sort the
    // transitive IDs before appending so the expanded set — and thus the final
    // installOrder — is deterministic across runs.
    let mut transitive_ids: Vec<&String> = graph
        .metadata
        .keys()
        .filter(|id| !declared_ids.contains(&id.as_str()))
        .collect();
    transitive_ids.sort_unstable();
    for fetched_id in transitive_ids {
        all_ids.push(fetched_id.clone());
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

    let mut depends_on_cycle: Option<Vec<String>> = None;
    let (ordered_ids, warnings) =
        compute_install_order(&order_input, override_order, &mut depends_on_cycle);

    if let Some(cycle) = depends_on_cycle {
        // `cycle` is the set of features in the cycle, not an ordered path —
        // render comma-separated so arrows don't imply a traversal order.
        return Err(format!(
            "cyclic dependsOn among features: {}",
            cycle.join(", ")
        ));
    }

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

    let mut depends_on_cycle: Option<Vec<String>> = None;
    let (ordered_ids, warnings) =
        compute_install_order(&order_input, override_order, &mut depends_on_cycle);

    if let Some(cycle) = depends_on_cycle {
        // `cycle` is the set of features in the cycle, not an ordered path —
        // render comma-separated so arrows don't imply a traversal order.
        return Err(format!("cyclic dependsOn among features: {}", cycle.join(", ")).into());
    }

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

/// Deduplicate the declared feature references, preserving first-seen order.
///
/// A devcontainer config may list the same feature twice; the official CLI
/// processes each unique reference once. Keeping declaration order makes the
/// fetch/traversal pass — and therefore the final `installOrder` — deterministic.
fn dedup_preserving_order(feature_pairs: &[(String, serde_json::Value)]) -> Vec<&str> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut result: Vec<&str> = Vec::new();
    for (reference, _) in feature_pairs {
        if seen.insert(reference.as_str()) {
            result.push(reference.as_str());
        }
    }
    result
}

/// Collect option values for transitively-pulled-in dependencies.
///
/// A feature's `dependsOn` map stores, per dependency reference, the option
/// values to install that dependency with (e.g.
/// `dependsOn: {".../common-utils:2": {"installZsh": true}}`). For features
/// the user never declared directly, those values are the only options we have
/// — so a transitive dep emits its real options instead of defaulting to `true`.
///
/// Earlier entries win (first parent to declare the dep), keeping the result
/// deterministic; iteration is over a `BTreeMap`-keyed metadata view.
fn collect_transitive_options(graph: &DependencyGraph) -> BTreeMap<String, serde_json::Value> {
    let mut options: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    // Iterate parents in a stable order so "first parent wins" is reproducible.
    let mut parent_ids: Vec<&String> = graph.metadata.keys().collect();
    parent_ids.sort_unstable();
    for parent_id in parent_ids {
        if let Some(meta) = graph.metadata.get(parent_id) {
            for (dep_id, dep_options) in &meta.depends_on {
                options
                    .entry(dep_id.clone())
                    .or_insert_with(|| dep_options.clone());
            }
        }
    }
    options
}

/// Build the `installOrder` entries from ordered IDs and their options.
///
/// Option resolution precedence:
/// 1. Explicit options from the user's `features` declaration (`feature_pairs`).
/// 2. Options carried by a parent's `dependsOn` value (`transitive_options`).
/// 3. `true` (the spec default) when neither is present.
fn build_install_order_entries(
    ordered_ids: &[String],
    feature_pairs: &[(String, serde_json::Value)],
    transitive_options: &BTreeMap<String, serde_json::Value>,
) -> Vec<InstallOrderEntry> {
    ordered_ids
        .iter()
        .map(|id| {
            let options = feature_pairs
                .iter()
                .find(|(ref_id, _)| ref_id == id)
                .map(|(_, opts)| opts.clone())
                .or_else(|| transitive_options.get(id).cloned())
                .unwrap_or(serde_json::Value::Bool(true));
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
        // A cycle (here via installs_after) must abort with an error, not fall
        // back to a partial order. compute_order_with_metadata surfaces the
        // cyclic-dependency warning as a fatal Err so the caller exits non-zero
        // without emitting JSON (matches the official CLI).
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

    /// Build graph metadata for a feature with a single hard `dependsOn`,
    /// where the dependency carries `dep_options` (mirrors real fetched data).
    fn graph_with_depends_on(
        feature: &str,
        dep: &str,
        dep_options: serde_json::Value,
    ) -> DependencyGraph {
        let mut meta_map = HashMap::new();
        meta_map.insert(
            feature.to_owned(),
            FeatureMetadata {
                id: feature.to_owned(),
                depends_on: std::iter::once((dep.to_owned(), dep_options)).collect(),
                ..Default::default()
            },
        );
        meta_map.insert(
            dep.to_owned(),
            FeatureMetadata {
                id: dep.to_owned(),
                ..Default::default()
            },
        );
        DependencyGraph {
            edges: vec![(feature.to_owned(), dep.to_owned(), EdgeKind::DependsOn)],
            metadata: meta_map,
        }
    }

    #[test]
    fn transitive_dep_appears_in_install_order() {
        // Scenario: config declares only `node`. `node` dependsOn `common-utils`.
        // After OCI fetch, `common-utils` must appear in installOrder AND be
        // ordered before node — the ordering crate now treats dependsOn as a
        // hard install-before prerequisite (regression: it previously only
        // honored installs_after, so a dependsOn-only target had no edge).
        let node = "ghcr.io/x/features/node:1";
        let common = "ghcr.io/x/features/common-utils:2";
        let pairs = vec![(node.to_owned(), serde_json::json!({"version": "lts"}))];

        let graph = graph_with_depends_on(node, common, serde_json::json!({}));

        let order = compute_order_with_metadata(&pairs, &graph, None).unwrap();

        assert!(
            order.contains(&common.to_owned()),
            "transitive dep common-utils must appear in installOrder; got: {order:?}"
        );
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(
            pos(common) < pos(node),
            "common-utils must be installed before node; order: {order:?}"
        );
    }

    #[test]
    fn transitive_dep_emits_parent_depends_on_options() {
        // node dependsOn common-utils with concrete options. common-utils is not
        // declared directly, so its installOrder entry must carry the options
        // from node's dependsOn value — not the `true` default. (Finding #2.)
        let node = "ghcr.io/x/features/node:1";
        let common = "ghcr.io/x/features/common-utils:2";
        let pairs = vec![(node.to_owned(), serde_json::json!({"version": "lts"}))];

        let dep_options = serde_json::json!({"installZsh": true});
        let graph = graph_with_depends_on(node, common, dep_options.clone());

        let order = compute_order_with_metadata(&pairs, &graph, None).unwrap();
        let transitive = collect_transitive_options(&graph);
        let entries = build_install_order_entries(&order, &pairs, &transitive);

        let common_entry = entries
            .iter()
            .find(|e| e.id == common)
            .expect("common-utils must have an installOrder entry");
        assert_eq!(
            common_entry.options, dep_options,
            "transitive dep must emit parent's dependsOn options, not `true`"
        );
        // The directly-declared feature still emits its explicit options.
        let node_entry = entries.iter().find(|e| e.id == node).unwrap();
        assert_eq!(node_entry.options, serde_json::json!({"version": "lts"}));
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

        let entries = build_install_order_entries(&ids, &pairs, &BTreeMap::new());

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

        let entries = build_install_order_entries(&ids, &pairs, &BTreeMap::new());

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].options, serde_json::Value::Bool(true));
    }

    #[test]
    fn install_entries_transitive_options_used_when_not_declared() {
        // node is declared with explicit options; common-utils is only pulled
        // in transitively, so its options come from transitive_options.
        let pairs = vec![(
            "ghcr.io/x/features/node:1".to_owned(),
            serde_json::json!({"version": "lts"}),
        )];
        let mut transitive = BTreeMap::new();
        transitive.insert(
            "ghcr.io/x/features/common-utils:2".to_owned(),
            serde_json::json!({"installZsh": true}),
        );
        let ids = vec![
            "ghcr.io/x/features/common-utils:2".to_owned(),
            "ghcr.io/x/features/node:1".to_owned(),
        ];

        let entries = build_install_order_entries(&ids, &pairs, &transitive);

        assert_eq!(entries[0].id, "ghcr.io/x/features/common-utils:2");
        assert_eq!(entries[0].options, serde_json::json!({"installZsh": true}));
        // Declared options take precedence over any transitive entry.
        assert_eq!(entries[1].options, serde_json::json!({"version": "lts"}));
    }

    #[test]
    fn install_entries_declared_options_win_over_transitive() {
        // If a feature is BOTH declared and a transitive dep, the user's
        // explicit declaration wins.
        let pairs = vec![(
            "ghcr.io/x/features/common-utils:2".to_owned(),
            serde_json::json!({"installZsh": false}),
        )];
        let mut transitive = BTreeMap::new();
        transitive.insert(
            "ghcr.io/x/features/common-utils:2".to_owned(),
            serde_json::json!({"installZsh": true}),
        );
        let ids = vec!["ghcr.io/x/features/common-utils:2".to_owned()];

        let entries = build_install_order_entries(&ids, &pairs, &transitive);

        assert_eq!(entries[0].options, serde_json::json!({"installZsh": false}));
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

    // -----------------------------------------------------------------------
    // dedup_preserving_order
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_preserving_order_keeps_first_seen_order() {
        let pairs = vec![
            ("b".to_owned(), serde_json::json!({})),
            ("a".to_owned(), serde_json::json!({})),
            ("b".to_owned(), serde_json::json!({"dup": true})),
            ("c".to_owned(), serde_json::json!({})),
        ];
        // Duplicate `b` is dropped; declaration order is otherwise preserved.
        assert_eq!(dedup_preserving_order(&pairs), vec!["b", "a", "c"]);
    }
}
