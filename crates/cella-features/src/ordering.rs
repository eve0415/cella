//! Dependency ordering for devcontainer features.
//!
//! Implements topological sort (Kahn's algorithm) over `installsAfter`
//! dependencies, with tiebreaking rules matching the devcontainer spec:
//!
//! 1. Official `ghcr.io/devcontainers/features/*` features sort first.
//! 2. Among equal-priority features, declaration order is preserved.
//! 3. `overrideFeatureInstallOrder` takes precedence over computed order.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::error::FeatureWarning;
use crate::types::FeatureMetadata;

/// Prefix for official devcontainer features in the OCI namespace.
const OFFICIAL_PREFIX: &str = "ghcr.io/devcontainers/features/";

/// Priority bucket for tiebreaking in the topological sort.
///
/// Lower numeric value = higher priority (installed first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey {
    /// 0 for official features, 1 for third-party.
    tier: u8,
    /// Index in the original declaration order.
    declaration_index: usize,
}

/// Compute the install order for a set of features.
///
/// Takes features in their declaration order and returns them reordered
/// according to `installsAfter` dependencies, official-first tiebreaking,
/// and any explicit override.
///
/// # Arguments
///
/// * `features` - Pairs of `(feature_id, metadata)` in declaration order.
/// * `override_order` - Optional explicit ordering from `overrideFeatureInstallOrder`.
///
/// # Returns
///
/// A tuple of `(ordered_ids, warnings)`. Warnings are emitted for cyclic
/// dependencies. On cycle detection, affected features fall back to
/// declaration order.
pub fn compute_install_order(
    features: &[(String, &FeatureMetadata)],
    override_order: Option<&[String]>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    if features.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut warnings = Vec::new();

    // Build lookup: id -> declaration index
    let id_to_index: HashMap<&str, usize> = features
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();

    // Determine whether any feature is official (for tiebreaking).
    let has_official = features
        .iter()
        .any(|(id, _)| id.starts_with(OFFICIAL_PREFIX));

    // Handle override ordering.
    if let Some(overrides) = override_order {
        return apply_override(
            features,
            overrides,
            &id_to_index,
            has_official,
            &mut warnings,
        );
    }

    // No override — run full topological sort.
    topological_sort(features, &id_to_index, has_official, &mut warnings)
}

/// Apply `overrideFeatureInstallOrder`: listed features go first in override
/// order (ignoring `installsAfter`), unlisted features are appended in
/// default topological sort order.
fn apply_override(
    features: &[(String, &FeatureMetadata)],
    overrides: &[String],
    id_to_index: &HashMap<&str, usize>,
    has_official: bool,
    warnings: &mut Vec<FeatureWarning>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    let override_set: HashSet<&str> = overrides.iter().map(String::as_str).collect();

    // Listed features in override order (skip any that aren't in our feature set).
    let mut result: Vec<String> = overrides
        .iter()
        .filter(|id| id_to_index.contains_key(id.as_str()))
        .cloned()
        .collect();

    // Unlisted features: topological sort among themselves only.
    let unlisted: Vec<(String, &FeatureMetadata)> = features
        .iter()
        .filter(|(id, _)| !override_set.contains(id.as_str()))
        .cloned()
        .collect();

    if !unlisted.is_empty() {
        let unlisted_id_to_index: HashMap<&str, usize> = unlisted
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();

        // For unlisted features, only consider dependencies among unlisted features.
        // Use the original declaration order for tiebreaking.
        let (unlisted_order, mut unlisted_warnings) = topological_sort_with_original_indices(
            &unlisted,
            &unlisted_id_to_index,
            id_to_index,
            has_official,
        );
        result.extend(unlisted_order);
        warnings.append(&mut unlisted_warnings);
    }

    (result, warnings.clone())
}

/// Topological sort using Kahn's algorithm with priority-queue tiebreaking.
fn topological_sort(
    features: &[(String, &FeatureMetadata)],
    id_to_index: &HashMap<&str, usize>,
    has_official: bool,
    warnings: &mut Vec<FeatureWarning>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    topological_sort_with_original_indices(features, id_to_index, id_to_index, has_official)
        .pipe_warnings(warnings)
}

/// Inner topological sort that accepts separate index maps for adjacency
/// lookup vs. tiebreaking (needed when sorting a subset of features while
/// preserving original declaration order).
fn topological_sort_with_original_indices(
    features: &[(String, &FeatureMetadata)],
    local_id_to_index: &HashMap<&str, usize>,
    original_id_to_index: &HashMap<&str, usize>,
    has_official: bool,
) -> (Vec<String>, Vec<FeatureWarning>) {
    let n = features.len();
    let mut warnings = Vec::new();

    // Adjacency: edge from dependency -> dependent (if dep installs after X,
    // then X must come before dep, so edge X -> dep).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_degree: Vec<usize> = vec![0; n];

    for (local_idx, (_, meta)) in features.iter().enumerate() {
        for dep_id in &meta.installs_after {
            if let Some(&dep_local_idx) = local_id_to_index.get(dep_id.as_str()) {
                adj[dep_local_idx].push(local_idx);
                in_degree[local_idx] += 1;
            }
            // Dependencies on features not in our set are ignored.
        }
    }

    // Priority queue: smallest SortKey wins (installed first).
    // BinaryHeap is max-heap, so wrap in Reverse.
    let mut heap: BinaryHeap<Reverse<(SortKey, usize)>> = BinaryHeap::new();

    for (local_idx, (id, _)) in features.iter().enumerate() {
        if in_degree[local_idx] == 0 {
            let key = sort_key(id, original_id_to_index, has_official);
            heap.push(Reverse((key, local_idx)));
        }
    }

    let mut result = Vec::with_capacity(n);

    while let Some(Reverse((_, local_idx))) = heap.pop() {
        let id = &features[local_idx].0;
        result.push(id.clone());

        for &neighbor in &adj[local_idx] {
            in_degree[neighbor] -= 1;
            if in_degree[neighbor] == 0 {
                let neighbor_id = &features[neighbor].0;
                let key = sort_key(neighbor_id, original_id_to_index, has_official);
                heap.push(Reverse((key, neighbor)));
            }
        }
    }

    // Cycle detection: if we couldn't process all nodes, remaining form a cycle.
    if result.len() < n {
        let processed: HashSet<String> = result.iter().cloned().collect();
        let cycle_members: Vec<String> = features
            .iter()
            .filter(|(id, _)| !processed.contains(id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();

        warnings.push(FeatureWarning::CyclicDependency {
            features: cycle_members,
        });

        // Append cyclic features in declaration order as fallback.
        for (id, _) in features {
            if !processed.contains(id.as_str()) {
                result.push(id.clone());
            }
        }
    }

    (result, warnings)
}

/// Compute the sort key for tiebreaking.
fn sort_key(id: &str, original_id_to_index: &HashMap<&str, usize>, has_official: bool) -> SortKey {
    let is_third_party = !(has_official && id.starts_with(OFFICIAL_PREFIX));
    let tier = u8::from(is_third_party);
    let declaration_index = original_id_to_index.get(id).copied().unwrap_or(usize::MAX);
    SortKey {
        tier,
        declaration_index,
    }
}

/// Helper trait to thread warnings through.
trait PipeWarnings {
    fn pipe_warnings(self, target: &mut Vec<FeatureWarning>) -> (Vec<String>, Vec<FeatureWarning>);
}

impl PipeWarnings for (Vec<String>, Vec<FeatureWarning>) {
    fn pipe_warnings(self, target: &mut Vec<FeatureWarning>) -> (Vec<String>, Vec<FeatureWarning>) {
        target.extend(self.1.clone());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FeatureMetadata;

    /// Helper to build a `FeatureMetadata` with only `installs_after` set.
    fn meta(id: &str, installs_after: &[&str]) -> FeatureMetadata {
        FeatureMetadata {
            id: id.to_string(),
            installs_after: installs_after.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    /// Helper to build a feature list from (id, metadata) tuples.
    fn feature_list(items: &[(String, FeatureMetadata)]) -> Vec<(String, &FeatureMetadata)> {
        items.iter().map(|(id, m)| (id.clone(), m)).collect()
    }

    // ---------------------------------------------------------------
    // No dependencies → declaration order preserved
    // ---------------------------------------------------------------

    #[test]
    fn no_dependencies_preserves_declaration_order() {
        let items = vec![
            ("alpha".to_string(), meta("alpha", &[])),
            ("beta".to_string(), meta("beta", &[])),
            ("gamma".to_string(), meta("gamma", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["alpha", "beta", "gamma"]);
    }

    // ---------------------------------------------------------------
    // Linear chain: A after B, B after C → C, B, A
    // ---------------------------------------------------------------

    #[test]
    fn linear_chain_respected() {
        let items = vec![
            ("a".to_string(), meta("a", &["b"])),
            ("b".to_string(), meta("b", &["c"])),
            ("c".to_string(), meta("c", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    // ---------------------------------------------------------------
    // Official features sort first when tiebreaking
    // ---------------------------------------------------------------

    #[test]
    fn official_features_sort_first_during_tiebreak() {
        let official_id = "ghcr.io/devcontainers/features/node";
        let third_party = "ghcr.io/someuser/features/foo";

        let items = vec![
            (third_party.to_string(), meta(third_party, &[])),
            (official_id.to_string(), meta(official_id, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        // Official sorts first despite being declared second.
        assert_eq!(order, vec![official_id, third_party]);
    }

    // ---------------------------------------------------------------
    // All third-party → pure declaration order (no official-first heuristic)
    // ---------------------------------------------------------------

    #[test]
    fn all_third_party_uses_declaration_order() {
        let items = vec![
            (
                "ghcr.io/user1/features/a".to_string(),
                meta("ghcr.io/user1/features/a", &[]),
            ),
            (
                "ghcr.io/user2/features/b".to_string(),
                meta("ghcr.io/user2/features/b", &[]),
            ),
            (
                "ghcr.io/user3/features/c".to_string(),
                meta("ghcr.io/user3/features/c", &[]),
            ),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        assert_eq!(
            order,
            vec![
                "ghcr.io/user1/features/a",
                "ghcr.io/user2/features/b",
                "ghcr.io/user3/features/c",
            ]
        );
    }

    // ---------------------------------------------------------------
    // Cycle: A after B, B after A → warning, fallback to declaration order
    // ---------------------------------------------------------------

    #[test]
    fn cycle_emits_warning_and_falls_back() {
        let items = vec![
            ("a".to_string(), meta("a", &["b"])),
            ("b".to_string(), meta("b", &["a"])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::CyclicDependency { features } => {
                assert!(features.contains(&"a".to_string()));
                assert!(features.contains(&"b".to_string()));
            }
            other => panic!("expected CyclicDependency, got {other:?}"),
        }
        // Cyclic features appended in declaration order.
        assert_eq!(order, vec!["a", "b"]);
    }

    // ---------------------------------------------------------------
    // Cycle with some non-cyclic features
    // ---------------------------------------------------------------

    #[test]
    fn partial_cycle_sorts_non_cyclic_first() {
        let items = vec![
            ("x".to_string(), meta("x", &[])),
            ("a".to_string(), meta("a", &["b"])),
            ("b".to_string(), meta("b", &["a"])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert_eq!(warnings.len(), 1);
        // x has no cycle, gets processed first.
        assert_eq!(order[0], "x");
        // a, b in declaration order as fallback.
        assert_eq!(&order[1..], &["a", "b"]);
    }

    // ---------------------------------------------------------------
    // overrideFeatureInstallOrder with explicit order
    // ---------------------------------------------------------------

    #[test]
    fn override_explicit_order() {
        let items = vec![
            ("a".to_string(), meta("a", &["b"])),
            ("b".to_string(), meta("b", &[])),
            ("c".to_string(), meta("c", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["c".to_string(), "a".to_string(), "b".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides));

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    // ---------------------------------------------------------------
    // Override with unlisted features → listed first, unlisted appended
    // ---------------------------------------------------------------

    #[test]
    fn override_with_unlisted_features() {
        let items = vec![
            ("a".to_string(), meta("a", &[])),
            ("b".to_string(), meta("b", &[])),
            ("c".to_string(), meta("c", &[])),
            ("d".to_string(), meta("d", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["c".to_string(), "a".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides));

        assert!(warnings.is_empty());
        // c, a listed first; b, d unlisted in declaration order.
        assert_eq!(order, vec!["c", "a", "b", "d"]);
    }

    // ---------------------------------------------------------------
    // Override ignores installsAfter for listed features
    // ---------------------------------------------------------------

    #[test]
    fn override_ignores_installs_after_for_listed() {
        // a installsAfter b, but override puts a before b.
        let items = vec![
            ("a".to_string(), meta("a", &["b"])),
            ("b".to_string(), meta("b", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["a".to_string(), "b".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides));

        assert!(warnings.is_empty());
        // Override order takes precedence over installsAfter.
        assert_eq!(order, vec!["a", "b"]);
    }

    // ---------------------------------------------------------------
    // Unlisted features with dependencies among themselves
    // ---------------------------------------------------------------

    #[test]
    fn override_unlisted_features_respect_installs_after() {
        let items = vec![
            ("listed".to_string(), meta("listed", &[])),
            ("x".to_string(), meta("x", &["y"])),
            ("y".to_string(), meta("y", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["listed".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides));

        assert!(warnings.is_empty());
        // listed first (override), then y before x (dependency).
        assert_eq!(order, vec!["listed", "y", "x"]);
    }

    // ---------------------------------------------------------------
    // Official tiebreaking with dependencies
    // ---------------------------------------------------------------

    #[test]
    fn official_tiebreak_with_dependencies() {
        let official = "ghcr.io/devcontainers/features/node";
        let third = "ghcr.io/someuser/features/tool";
        let another = "ghcr.io/anotheruser/features/util";

        // third depends on another, official has no deps.
        let items = vec![
            (third.to_string(), meta(third, &[another])),
            (official.to_string(), meta(official, &[])),
            (another.to_string(), meta(another, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        // official sorts first (tier 0), then another (tier 1, index 2),
        // then third (tier 1, depends on another).
        assert_eq!(order, vec![official, another, third]);
    }

    // ---------------------------------------------------------------
    // Empty input
    // ---------------------------------------------------------------

    #[test]
    fn empty_input_returns_empty() {
        let features: Vec<(String, &FeatureMetadata)> = vec![];
        let (order, warnings) = compute_install_order(&features, None);
        assert!(order.is_empty());
        assert!(warnings.is_empty());
    }

    // ---------------------------------------------------------------
    // Single feature
    // ---------------------------------------------------------------

    #[test]
    fn single_feature_returned_as_is() {
        let items = vec![("only".to_string(), meta("only", &[]))];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["only"]);
    }

    // ---------------------------------------------------------------
    // Dependency on unknown feature is silently ignored
    // ---------------------------------------------------------------

    #[test]
    fn unknown_dependency_ignored() {
        let items = vec![
            ("a".to_string(), meta("a", &["nonexistent"])),
            ("b".to_string(), meta("b", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        // a's dependency on nonexistent is ignored, so declaration order.
        assert_eq!(order, vec!["a", "b"]);
    }

    // ---------------------------------------------------------------
    // Diamond dependency: A→B, A→C, B→D, C→D
    // ---------------------------------------------------------------

    #[test]
    fn diamond_dependency() {
        let items = vec![
            ("a".to_string(), meta("a", &["b", "c"])),
            ("b".to_string(), meta("b", &["d"])),
            ("c".to_string(), meta("c", &["d"])),
            ("d".to_string(), meta("d", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None);

        assert!(warnings.is_empty());
        // d must come before b and c; b and c before a.
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("d") < pos("b"));
        assert!(pos("d") < pos("c"));
        assert!(pos("b") < pos("a"));
        assert!(pos("c") < pos("a"));
    }

    // ---------------------------------------------------------------
    // Override references a feature not in our set (silently skipped)
    // ---------------------------------------------------------------

    #[test]
    fn override_with_unknown_feature_id_skipped() {
        let items = vec![
            ("a".to_string(), meta("a", &[])),
            ("b".to_string(), meta("b", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["nonexistent".to_string(), "b".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides));

        assert!(warnings.is_empty());
        // b listed (override), a unlisted (appended).
        assert_eq!(order, vec!["b", "a"]);
    }
}
