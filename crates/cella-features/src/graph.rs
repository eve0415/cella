//! Dependency graph builder and Mermaid renderer for devcontainer features.
//!
//! Builds the dependency graph by recursively fetching and parsing
//! `devcontainer-feature.json` from OCI registries, then renders the result as
//! a Mermaid `flowchart TD` diagram.
//!
//! Two edge kinds are modelled to match the official devcontainer CLI renderer:
//! - **`dependsOn`** (hard dependency) → solid arrow `A --> B`
//! - **`installsAfter`** (soft ordering hint) → dashed arrow `A -.-> B`

use std::collections::{HashMap, HashSet};

use tracing::debug;

use crate::FeatureError;
use crate::cache::FeatureCache;
use crate::metadata::parse_feature_metadata;
use crate::oci::{FeatureFetcher, OciFetcher};
use crate::reference::NormalizedRef;
use crate::types::Platform;

/// The kind of dependency relationship represented by a graph edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Hard dependency declared via `dependsOn` — rendered as a solid arrow `-->`.
    DependsOn,
    /// Soft ordering hint declared via `installsAfter` — rendered as a dashed arrow `-.->`.
    InstallsAfter,
}

/// An edge in the feature dependency graph.
///
/// `(from_ref, to_ref, kind)` means `from_ref` depends on (or installs after) `to_ref`.
pub type DepEdge = (String, String, EdgeKind);

/// The result of building a dependency graph from a set of root feature references.
pub struct DependencyGraph {
    /// All edges discovered by recursive OCI fetch.
    pub edges: Vec<DepEdge>,
    /// Fetched metadata for every feature visited (roots + all transitives).
    ///
    /// Keys are the feature reference strings as declared (roots) or as
    /// referenced in `dependsOn` (transitives).
    pub metadata: HashMap<String, crate::FeatureMetadata>,
}

/// Recursively build the dependency graph starting from one or more root refs.
///
/// Each root is visited once; transitive `dependsOn` targets are recursed into
/// and their metadata is collected. `installsAfter` edges are recorded but not
/// walked (matching official CLI behaviour).
///
/// Emits "Building dependency graph..." to stderr before starting.
///
/// # Errors
///
/// Returns an error when a feature reference cannot be parsed, normalised, or
/// fetched. Transitive errors are propagated.
pub async fn build_dependency_graph(root_refs: &[&str]) -> Result<DependencyGraph, FeatureError> {
    eprintln!("Building dependency graph...");

    let cache = FeatureCache::new();
    let fetcher = OciFetcher::new();
    let platform = host_platform();

    let mut edges: Vec<DepEdge> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut metadata: HashMap<String, crate::FeatureMetadata> = HashMap::new();

    for root_ref in root_refs {
        visit(
            root_ref,
            &fetcher,
            &platform,
            &cache,
            &mut edges,
            &mut visited,
            &mut metadata,
        )
        .await?;
    }

    Ok(DependencyGraph { edges, metadata })
}

/// Render a dependency graph as a Mermaid `flowchart TD` diagram.
///
/// Matches the official devcontainer CLI's `generateMermaidDiagram`: each
/// user-provided root gets its own entry at the top of the diagram, with edges
/// from all roots rendered beneath it. Node labels use the short form
/// `name:tag` (last path segment + tag).
///
/// When `edges` is empty for a given root, that root is rendered as an isolated
/// node. The official CLI uses `flowchart` (no `TD`); we match that.
///
/// Edge rendering:
/// - `dependsOn` → solid arrow `A --> B`
/// - `installsAfter` → dashed arrow `A -.-> B`
pub fn render_mermaid(roots: &[&str], edges: &[DepEdge]) -> String {
    let mut node_ids: HashMap<String, String> = HashMap::new();
    let mut next_id = 0usize;

    // Assign stable short node IDs; first call for a ref reserves the next slot.
    let mut get_id = |ref_str: &str| -> String {
        node_ids
            .entry(ref_str.to_owned())
            .or_insert_with(|| {
                let id = node_label_from_id(next_id);
                next_id += 1;
                id
            })
            .clone()
    };

    // Reserve IDs for all roots first so they get the lowest-numbered slots.
    for root in roots {
        get_id(root);
    }

    // Official CLI uses `flowchart` (no direction keyword).
    let mut lines: Vec<String> = vec!["flowchart TD".to_owned()];

    // Track which roots have at least one outgoing edge so we can emit isolated
    // nodes for those that don't.
    let roots_with_edges: HashSet<&str> = edges.iter().map(|(from, _, _)| from.as_str()).collect();

    for root in roots {
        if !roots_with_edges.contains(*root) {
            let id = get_id(root);
            let label = short_label(root);
            lines.push(format!("    {id}[\"{label}\"]"));
        }
    }

    for (from, to, kind) in edges {
        let from_id = get_id(from);
        let to_id = get_id(to);
        let from_label = short_label(from);
        let to_label = short_label(to);
        let arrow = match kind {
            EdgeKind::DependsOn => "-->",
            EdgeKind::InstallsAfter => "-.->",
        };
        lines.push(format!(
            "    {from_id}[\"{from_label}\"] {arrow} {to_id}[\"{to_label}\"]"
        ));
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Produce a short human-readable label: `name:tag` from a full OCI ref.
fn short_label(reference: &str) -> String {
    // Strip registry: everything after the first `/`
    let after_registry = reference.split_once('/').map_or(reference, |(_, r)| r);
    // Last path segment (the feature name), possibly with a tag
    after_registry
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .to_owned()
}

/// Generate a deterministic Mermaid node identifier from a sequential index.
///
/// Produces `A`, `B`, …, `Z`, `AA`, `AB`, … — short and legible in diagrams.
fn node_label_from_id(index: usize) -> String {
    let mut n = index;
    let mut label = String::new();
    loop {
        let remainder = n % 26;
        label.insert(0, (b'A' + u8::try_from(remainder).unwrap_or(0)) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    label
}

/// Determine the host platform for feature fetching.
fn host_platform() -> Platform {
    let os = std::env::consts::OS;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    Platform {
        os: os.to_owned(),
        architecture: arch.to_owned(),
    }
}

/// Recursively visit a feature reference, collecting edges and metadata.
async fn visit(
    reference: &str,
    fetcher: &OciFetcher,
    platform: &Platform,
    cache: &FeatureCache,
    edges: &mut Vec<DepEdge>,
    visited: &mut HashSet<String>,
    metadata_map: &mut HashMap<String, crate::FeatureMetadata>,
) -> Result<(), FeatureError> {
    if !visited.insert(reference.to_owned()) {
        debug!("graph: already visited {reference}, skipping");
        return Ok(());
    }

    let norm_ref = parse_norm_ref(reference)?;

    let artifact_dir = fetcher.fetch(&norm_ref, platform, cache).await?;

    let metadata_path = artifact_dir.join("devcontainer-feature.json");
    let json =
        std::fs::read_to_string(&metadata_path).map_err(|e| FeatureError::InvalidMetadata {
            feature_id: reference.to_owned(),
            reason: format!("cannot read devcontainer-feature.json: {e}"),
        })?;

    let metadata = parse_feature_metadata(&json)?;

    // Hard dependencies: recurse into each and emit a solid edge.
    let dep_refs: Vec<String> = metadata.depends_on.keys().cloned().collect();
    for dep_ref in &dep_refs {
        edges.push((reference.to_owned(), dep_ref.clone(), EdgeKind::DependsOn));
        // Box the recursive future to avoid infinitely-sized stack frames.
        Box::pin(visit(
            dep_ref,
            fetcher,
            platform,
            cache,
            edges,
            visited,
            metadata_map,
        ))
        .await?;
    }

    // Soft ordering hints: record the edge but do not recurse.
    for dep_ref in &metadata.installs_after {
        edges.push((
            reference.to_owned(),
            dep_ref.clone(),
            EdgeKind::InstallsAfter,
        ));
    }

    metadata_map.insert(reference.to_owned(), metadata);

    Ok(())
}

/// Parse a plain OCI reference string into a [`NormalizedRef`].
fn parse_norm_ref(reference: &str) -> Result<NormalizedRef, FeatureError> {
    let (registry, rest) =
        reference
            .split_once('/')
            .ok_or_else(|| FeatureError::InvalidReference {
                reference: reference.to_owned(),
                reason: "expected registry/repository format".to_owned(),
            })?;

    let (repository, tag) = rest.rsplit_once(':').map_or_else(
        || (rest.to_owned(), "latest".to_owned()),
        |(r, t)| (r.to_owned(), t.to_owned()),
    );

    Ok(NormalizedRef::OciTarget {
        registry: registry.to_owned(),
        repository,
        tag,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // short_label
    // -----------------------------------------------------------------------

    #[test]
    fn short_label_full_ref() {
        assert_eq!(
            short_label("ghcr.io/devcontainers/features/node:1"),
            "node:1"
        );
    }

    #[test]
    fn short_label_no_registry() {
        assert_eq!(short_label("features/node:1"), "node:1");
    }

    #[test]
    fn short_label_no_tag() {
        assert_eq!(short_label("ghcr.io/devcontainers/features/node"), "node");
    }

    // -----------------------------------------------------------------------
    // node_label_from_id
    // -----------------------------------------------------------------------

    #[test]
    fn node_ids_sequential() {
        assert_eq!(node_label_from_id(0), "A");
        assert_eq!(node_label_from_id(1), "B");
        assert_eq!(node_label_from_id(25), "Z");
        assert_eq!(node_label_from_id(26), "AA");
        assert_eq!(node_label_from_id(27), "AB");
    }

    // -----------------------------------------------------------------------
    // render_mermaid
    // -----------------------------------------------------------------------

    #[test]
    fn render_mermaid_no_edges() {
        let root = "ghcr.io/devcontainers/features/node:1";
        let output = render_mermaid(&[root], &[]);
        assert!(output.starts_with("flowchart TD"));
        assert!(output.contains("A[\"node:1\"]"));
        // No arrow present
        assert!(!output.contains("-->"));
    }

    #[test]
    fn render_mermaid_depends_on_edge() {
        let root = "ghcr.io/devcontainers/features/node:1";
        let edges = vec![(
            root.to_owned(),
            "ghcr.io/devcontainers/features/common-utils:2".to_owned(),
            EdgeKind::DependsOn,
        )];
        let output = render_mermaid(&[root], &edges);
        assert!(output.starts_with("flowchart TD"));
        // Hard dep → solid arrow
        assert!(output.contains("-->"), "expected solid arrow");
        assert!(!output.contains("-.-"), "unexpected dashed arrow");
        assert!(output.contains("node:1"));
        assert!(output.contains("common-utils:2"));
    }

    #[test]
    fn render_mermaid_installs_after_edge() {
        let root = "ghcr.io/devcontainers/features/node:1";
        let edges = vec![(
            root.to_owned(),
            "ghcr.io/devcontainers/features/common-utils:2".to_owned(),
            EdgeKind::InstallsAfter,
        )];
        let output = render_mermaid(&[root], &edges);
        assert!(output.starts_with("flowchart TD"));
        // Soft dep → dashed arrow
        assert!(output.contains("-.-"), "expected dashed arrow");
        assert!(output.contains("node:1"));
        assert!(output.contains("common-utils:2"));
    }

    #[test]
    fn render_mermaid_mixed_edges() {
        let root = "ghcr.io/devcontainers/features/node:1";
        let edges = vec![
            (
                root.to_owned(),
                "ghcr.io/devcontainers/features/git:1".to_owned(),
                EdgeKind::DependsOn,
            ),
            (
                root.to_owned(),
                "ghcr.io/devcontainers/features/common-utils:2".to_owned(),
                EdgeKind::InstallsAfter,
            ),
        ];
        let output = render_mermaid(&[root], &edges);
        // Header + 2 edge lines
        assert_eq!(output.lines().count(), 3);
        assert!(output.contains("-->"), "expected solid arrow for dependsOn");
        assert!(
            output.contains("-.-"),
            "expected dashed arrow for installsAfter"
        );
        assert!(output.contains("node:1"));
        assert!(output.contains("git:1"));
        assert!(output.contains("common-utils:2"));
    }

    #[test]
    fn render_mermaid_multiple_roots_no_edges() {
        let roots = [
            "ghcr.io/devcontainers/features/node:1",
            "ghcr.io/devcontainers/features/python:1",
        ];
        let output = render_mermaid(&roots, &[]);
        assert!(output.starts_with("flowchart TD"));
        // Both roots appear as isolated nodes
        assert!(output.contains("node:1"), "node root missing");
        assert!(output.contains("python:1"), "python root missing");
        assert!(!output.contains("-->"));
    }

    #[test]
    fn render_mermaid_multiple_roots_with_shared_dep() {
        let node = "ghcr.io/devcontainers/features/node:1";
        let python = "ghcr.io/devcontainers/features/python:1";
        let common = "ghcr.io/devcontainers/features/common-utils:2";
        let edges = vec![
            (node.to_owned(), common.to_owned(), EdgeKind::DependsOn),
            (python.to_owned(), common.to_owned(), EdgeKind::DependsOn),
        ];
        let output = render_mermaid(&[node, python], &edges);
        assert!(output.starts_with("flowchart TD"));
        assert!(output.contains("node:1"));
        assert!(output.contains("python:1"));
        assert!(output.contains("common-utils:2"));
        // Both roots should have solid arrows to common-utils
        assert_eq!(output.matches("-->").count(), 2);
    }

    // -----------------------------------------------------------------------
    // parse_norm_ref
    // -----------------------------------------------------------------------

    #[test]
    fn parse_norm_ref_valid() {
        let result = parse_norm_ref("ghcr.io/devcontainers/features/node:1").unwrap();
        match result {
            NormalizedRef::OciTarget {
                registry,
                repository,
                tag,
            } => {
                assert_eq!(registry, "ghcr.io");
                assert_eq!(repository, "devcontainers/features/node");
                assert_eq!(tag, "1");
            }
            _ => panic!("expected OciTarget"),
        }
    }

    #[test]
    fn parse_norm_ref_no_slash_errors() {
        assert!(parse_norm_ref("not-valid").is_err());
    }
}
