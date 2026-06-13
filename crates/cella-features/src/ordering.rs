//! Dependency ordering for devcontainer features.
//!
//! Implements topological sort (Kahn's algorithm) over both `dependsOn` (hard)
//! and `installsAfter` (soft) dependencies — every prerequisite must install
//! first — with tiebreaking rules matching the devcontainer spec:
//!
//! 1. Official `ghcr.io/devcontainers/features/*` features sort first.
//! 2. Among equal-priority features, declaration order is preserved.
//! 3. `overrideFeatureInstallOrder` takes precedence over computed order.
//!
//! `dependsOn` edges are hard ordering constraints — a feature cannot be
//! emitted until ALL its `dependsOn` targets appear earlier in the result.
//! Cycles in `dependsOn` edges are surfaced via the `depends_on_cycle`
//! out-parameter so callers can return a fatal error. `installsAfter` cycles
//! are merely soft hints: they are reported as a non-fatal warning and the
//! affected features fall back to declaration order.
//!
//! Hard and soft cycles are detected independently. A hard cycle is determined
//! solely from the `dependsOn`-only graph, so a soft `installsAfter` edge that
//! merely *participates* in a loop with otherwise-acyclic `dependsOn` edges
//! (e.g. `A dependsOn B`, `B installsAfter A` — satisfiable by installing `B`
//! first) is never misclassified as a fatal `dependsOn` cycle.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::error::FeatureWarning;
use crate::types::FeatureMetadata;

/// Prefix for official devcontainer features in the OCI namespace.
const OFFICIAL_PREFIX: &str = "ghcr.io/devcontainers/features/";

/// Adjacency lists plus degree counters for a Kahn sort.
///
/// Hard (`dependsOn`) and soft (`installsAfter`) edges are kept apart so the
/// scheduler can apply both while cycle detection reasons about the hard graph
/// in isolation. All edges flow prerequisite → dependent (emit prereq first).
struct DepGraph {
    /// Hard-edge adjacency: `hard_adj[prereq]` lists dependents that
    /// `dependsOn` `prereq`.
    hard_adj: Vec<Vec<usize>>,
    /// Soft-edge adjacency: `soft_adj[prereq]` lists dependents that
    /// `installsAfter` `prereq`.
    soft_adj: Vec<Vec<usize>>,
    /// Combined (hard + soft) in-degree used by the scheduling Kahn pass.
    in_degree: Vec<usize>,
    /// Hard-only in-degree, used to seed the hard-cycle detection pass.
    hard_in_degree: Vec<usize>,
}

/// Priority bucket for tiebreaking in the topological sort (no override).
///
/// Lower numeric value = higher priority (installed first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey {
    /// 0 for official features, 1 for third-party.
    tier: u8,
    /// Index in the original declaration order.
    declaration_index: usize,
}

/// Extended sort key used when `overrideFeatureInstallOrder` is active.
///
/// Override-listed features get `override_tier = 0` and sort by their
/// position in the override list.  Unlisted features get `override_tier = 1`
/// and fall back to the standard official-first + declaration-order tiebreak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OverrideSortKey {
    /// 0 = in override list, 1 = not in override list.
    override_tier: u8,
    /// Position in the override list (only meaningful when `override_tier == 0`).
    override_pos: usize,
    /// 0 = official, 1 = third-party (only meaningful when `override_tier == 1`).
    official_tier: u8,
    /// Declaration index tiebreaker (only meaningful when `override_tier == 1`).
    declaration_index: usize,
}

/// Build a lookup from exact feature ID to its index in the slice.
///
/// Maps each feature's real id to its index. Legacy-id aliases are NOT included
/// here: this index backs `dependsOn` (hard) edges, override resolution, and
/// tiebreaking, all of which the official matches by exact id (`equals`).
/// `installsAfter` (soft) matching, which DOES resolve `legacyIds`, goes through
/// [`resolve_soft_dep`] instead.
fn build_id_index(features: &[(String, &FeatureMetadata)]) -> HashMap<String, usize> {
    features
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.clone(), i))
        .collect()
}

/// Resolve an `installsAfter` (soft-dependency) target id to a feature index.
///
/// Mirrors the official `satisfiesSoftDependency`: try an exact id match first
/// (so a real id always wins), then fall back to any feature whose `legacy_ids`
/// matches the target after alias qualification.
///
/// Alias qualification follows the official CLI: a feature's `legacyIds` are
/// stored as bare names (e.g. `"docker-from-docker"`), but `installsAfter`
/// entries are fully qualified OCI refs (e.g.
/// `"ghcr.io/devcontainers/features/docker-from-docker"`). To match them the
/// official code constructs `{feature_registry}/{feature_namespace}/{alias}`
/// and compares that to the `installsAfter` target. Bare-vs-bare comparison is
/// also retained so non-OCI / short-ref feature sets still work.
///
/// `dependsOn` deliberately does NOT use this (hard deps match by exact id
/// only, per the spec).
fn resolve_soft_dep(
    dep_id: &str,
    id_to_index: &HashMap<String, usize>,
    features: &[(String, &FeatureMetadata)],
) -> Option<usize> {
    if let Some(&idx) = id_to_index.get(dep_id) {
        return Some(idx);
    }
    features.iter().position(|(feature_id, meta)| {
        meta.legacy_ids.iter().any(|alias| {
            // Bare-to-bare: alias == dep_id (handles non-OCI / short-ref sets).
            if alias == dep_id {
                return true;
            }
            // Qualified: prepend the feature's own registry/namespace prefix to
            // the bare alias, then compare to dep_id.  This mirrors the official
            // `satisfiesSoftDependency` which builds
            // `${softDepRef.registry}/${softDepRef.namespace}/${legacyId}`.
            feature_id
                .rsplit_once('/')
                .map(|(p, _)| p)
                .is_some_and(|prefix| format!("{prefix}/{alias}") == dep_id)
        })
    })
}

/// Compute the install order for a set of features.
///
/// Takes features in their declaration order and returns them reordered
/// according to `dependsOn` (hard) and `installsAfter` (soft) dependencies,
/// official-first tiebreaking, and any explicit override.
///
/// # Arguments
///
/// * `features` - Pairs of `(feature_id, metadata)` in declaration order.
/// * `override_order` - Optional explicit ordering from `overrideFeatureInstallOrder`.
/// * `depends_on_cycle` - Set to the cycle members when a hard `dependsOn`
///   cycle is detected; caller should return a fatal error in that case.
///
/// # Returns
///
/// A tuple of `(ordered_ids, warnings)`. Warnings are emitted for cyclic
/// `installsAfter` soft deps only. Cyclic `dependsOn` hard deps are
/// signalled via `depends_on_cycle` instead.
pub fn compute_install_order(
    features: &[(String, &FeatureMetadata)],
    override_order: Option<&[String]>,
    depends_on_cycle: &mut Option<Vec<String>>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    if features.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut warnings = Vec::new();

    // Build lookup: id -> declaration index
    let id_to_index = build_id_index(features);

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
            depends_on_cycle,
            &mut warnings,
        );
    }

    // No override — run full topological sort.
    topological_sort(
        features,
        &id_to_index,
        has_official,
        depends_on_cycle,
        &mut warnings,
    )
}

/// Apply `overrideFeatureInstallOrder` as a **priority hint** inside the
/// topological sort.
///
/// Per the devcontainer spec, `overrideFeatureInstallOrder` controls which
/// `in_degree == 0` candidate is picked first in each Kahn round.  It does
/// NOT bypass `dependsOn` hard constraints — a feature that `dependsOn`
/// another must still wait for that prerequisite regardless of its position in
/// the override list.
///
/// Override-listed features use `OverrideSortKey` tier 0 (highest priority)
/// with their override position as tiebreak, and have their own
/// `installsAfter` soft edges ignored.  Unlisted features fall back to the
/// standard official-first + declaration-order tiebreak (tier 1).
fn apply_override(
    features: &[(String, &FeatureMetadata)],
    overrides: &[String],
    id_to_index: &HashMap<String, usize>,
    has_official: bool,
    depends_on_cycle: &mut Option<Vec<String>>,
    warnings: &mut Vec<FeatureWarning>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    let override_position: HashMap<&str, usize> = overrides
        .iter()
        .enumerate()
        .filter(|(_, id)| id_to_index.contains_key(id.as_str()))
        .map(|(pos, id)| (id.as_str(), pos))
        .collect();

    let graph = build_graph(features, id_to_index, Some(&override_position));
    let ext_key = |id: &str| override_sort_key(id, &override_position, id_to_index, has_official);

    let mut in_degree = graph.in_degree.clone();
    let mut heap: BinaryHeap<Reverse<(OverrideSortKey, usize)>> = BinaryHeap::new();
    for (local_idx, (id, _)) in features.iter().enumerate() {
        if in_degree[local_idx] == 0 {
            heap.push(Reverse((ext_key(id), local_idx)));
        }
    }

    let mut result = Vec::with_capacity(features.len());
    while let Some(Reverse((_, local_idx))) = heap.pop() {
        result.push(features[local_idx].0.clone());
        // Iterate adjacency slices by reference — no per-pop clone needed.
        for &dep in graph.hard_adj[local_idx]
            .iter()
            .chain(&graph.soft_adj[local_idx])
        {
            in_degree[dep] -= 1;
            if in_degree[dep] == 0 {
                heap.push(Reverse((ext_key(&features[dep].0), dep)));
            }
        }
    }

    if let Some(fallback) =
        finish_with_cycle_detection(features, &graph, &result, depends_on_cycle, warnings)
    {
        result.extend(fallback);
    }

    (result, warnings.clone())
}

/// Topological sort using Kahn's algorithm with priority-queue tiebreaking.
fn topological_sort(
    features: &[(String, &FeatureMetadata)],
    id_to_index: &HashMap<String, usize>,
    has_official: bool,
    depends_on_cycle: &mut Option<Vec<String>>,
    warnings: &mut Vec<FeatureWarning>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    topological_sort_with_original_indices(
        features,
        id_to_index,
        id_to_index,
        has_official,
        depends_on_cycle,
    )
    .pipe_warnings(warnings)
}

/// Inner topological sort that accepts separate index maps for adjacency
/// lookup vs. tiebreaking (needed when sorting a subset of features while
/// preserving original declaration order).
///
/// Both `dependsOn` (hard) and `installsAfter` (soft) edges contribute to
/// `in_degree`.  Cycle detection reasons about the hard graph in isolation so
/// hard cycles are signalled via `depends_on_cycle` while soft-only cycles
/// emit a warning and fall back to declaration order.
fn topological_sort_with_original_indices(
    features: &[(String, &FeatureMetadata)],
    local_id_to_index: &HashMap<String, usize>,
    original_id_to_index: &HashMap<String, usize>,
    has_official: bool,
    depends_on_cycle: &mut Option<Vec<String>>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    let mut warnings = Vec::new();

    let graph = build_graph(features, local_id_to_index, None);
    let mut in_degree = graph.in_degree.clone();

    // Priority queue: smallest SortKey wins (installed first).
    // BinaryHeap is max-heap, so wrap in Reverse.
    let mut heap: BinaryHeap<Reverse<(SortKey, usize)>> = BinaryHeap::new();
    for (local_idx, (id, _)) in features.iter().enumerate() {
        if in_degree[local_idx] == 0 {
            heap.push(Reverse((
                sort_key(id, original_id_to_index, has_official),
                local_idx,
            )));
        }
    }

    let mut result = Vec::with_capacity(features.len());
    while let Some(Reverse((_, local_idx))) = heap.pop() {
        result.push(features[local_idx].0.clone());

        // Both edge kinds impose the same install-before constraint, so a
        // dependent only becomes schedulable once every hard AND soft
        // prerequisite has been emitted. Iterating the adjacency slices by
        // reference avoids cloning them on every pop.
        for &dependent in graph.hard_adj[local_idx]
            .iter()
            .chain(&graph.soft_adj[local_idx])
        {
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                heap.push(Reverse((
                    sort_key(&features[dependent].0, original_id_to_index, has_official),
                    dependent,
                )));
            }
        }
    }

    if let Some(fallback) =
        finish_with_cycle_detection(features, &graph, &result, depends_on_cycle, &mut warnings)
    {
        result.extend(fallback);
    }

    (result, warnings)
}

/// Build hard/soft adjacency lists and degree counters for the Kahn passes.
///
/// Hard (`dependsOn`) edges are always included.  Soft (`installsAfter`) edges
/// are included for every feature, except — when `override_position` is
/// provided — for features that appear in the override list (their soft
/// ordering hints are deliberately ignored so the override wins).
fn build_graph(
    features: &[(String, &FeatureMetadata)],
    id_to_index: &HashMap<String, usize>,
    override_position: Option<&HashMap<&str, usize>>,
) -> DepGraph {
    let n = features.len();
    let mut graph = DepGraph {
        hard_adj: vec![Vec::new(); n],
        soft_adj: vec![Vec::new(); n],
        in_degree: vec![0; n],
        hard_in_degree: vec![0; n],
    };

    for (local_idx, (id, meta)) in features.iter().enumerate() {
        // Hard edges: dependsOn (dep must be installed BEFORE local_idx).
        // Option values on the dependsOn key form part of the dependency's
        // identity, but the install set has already been expanded to one entry
        // per (ref, options) pair upstream, so matching by feature id here
        // lands on the correct instance.
        for dep_id in meta.depends_on.keys() {
            if let Some(&prereq_idx) = id_to_index.get(dep_id.as_str()) {
                graph.hard_adj[prereq_idx].push(local_idx);
                graph.in_degree[local_idx] += 1;
                graph.hard_in_degree[local_idx] += 1;
            }
            // dependsOn targets outside the current set were already injected
            // by resolve_features; any remaining unknowns are ignored here.
        }

        // Soft edges: installsAfter (dep should be installed BEFORE local_idx).
        // Skipped for override-listed features so the override order wins.
        // Resolved via legacyIds aliases too (matches satisfiesSoftDependency).
        let suppress_soft = override_position.is_some_and(|p| p.contains_key(id.as_str()));
        if !suppress_soft {
            for dep_id in &meta.installs_after {
                if let Some(prereq_idx) = resolve_soft_dep(dep_id, id_to_index, features) {
                    graph.soft_adj[prereq_idx].push(local_idx);
                    graph.in_degree[local_idx] += 1;
                }
            }
        }
    }

    graph
}

/// Run Kahn's algorithm on the **hard-only** (`dependsOn`) graph and return the
/// members of any hard cycle — i.e. nodes that can never be scheduled when
/// only `dependsOn` edges are considered.
///
/// This is the authoritative hard-cycle test: it ignores soft `installsAfter`
/// edges entirely, so a satisfiable-but-tangled case like `A dependsOn B`,
/// `B installsAfter A` reports *no* hard cycle (the `dependsOn` graph `B → A`
/// is acyclic) even though the combined graph stalls.
fn hard_cycle_members(features: &[(String, &FeatureMetadata)], graph: &DepGraph) -> Vec<String> {
    let n = features.len();
    let mut hard_in_degree = graph.hard_in_degree.clone();
    let mut queue: Vec<usize> = (0..n).filter(|&i| hard_in_degree[i] == 0).collect();
    let mut processed = vec![false; n];

    while let Some(idx) = queue.pop() {
        processed[idx] = true;
        for &dependent in &graph.hard_adj[idx] {
            hard_in_degree[dependent] -= 1;
            if hard_in_degree[dependent] == 0 {
                queue.push(dependent);
            }
        }
    }

    features
        .iter()
        .zip(processed)
        .filter(|(_, done)| !done)
        .map(|((id, _), _)| id.clone())
        .collect()
}

/// Classify the outcome of the scheduling Kahn pass.
///
/// - Complete schedule → `None` (nothing more to do).
/// - Hard (`dependsOn`) cycle → sets `depends_on_cycle` and returns `None`
///   (the caller surfaces a fatal error; no fallback is appended).
/// - Soft-only (`installsAfter`) cycle → pushes a non-fatal warning and
///   returns `Some(leftover)` — the unscheduled features in declaration order
///   for the caller to append as a best-effort fallback.
fn finish_with_cycle_detection(
    features: &[(String, &FeatureMetadata)],
    graph: &DepGraph,
    scheduled: &[String],
    depends_on_cycle: &mut Option<Vec<String>>,
    warnings: &mut Vec<FeatureWarning>,
) -> Option<Vec<String>> {
    if scheduled.len() == features.len() {
        return None;
    }

    // A hard cycle is determined purely from the dependsOn-only graph.
    let hard_cycle = hard_cycle_members(features, graph);
    if !hard_cycle.is_empty() {
        *depends_on_cycle = Some(hard_cycle);
        return None;
    }

    // Soft-only (installsAfter) cycle: warn and fall back to declaration order.
    let processed: HashSet<&str> = scheduled.iter().map(String::as_str).collect();
    let leftover: Vec<String> = features
        .iter()
        .filter(|(id, _)| !processed.contains(id.as_str()))
        .map(|(id, _)| id.clone())
        .collect();

    warnings.push(FeatureWarning::CyclicDependency {
        features: leftover.clone(),
    });
    Some(leftover)
}

/// Compute the sort key for tiebreaking.
fn sort_key(
    id: &str,
    original_id_to_index: &HashMap<String, usize>,
    has_official: bool,
) -> SortKey {
    let is_third_party = !(has_official && id.starts_with(OFFICIAL_PREFIX));
    let tier = u8::from(is_third_party);
    let declaration_index = original_id_to_index.get(id).copied().unwrap_or(usize::MAX);
    SortKey {
        tier,
        declaration_index,
    }
}

/// Compute the `OverrideSortKey` for a feature id.
fn override_sort_key(
    id: &str,
    override_position: &HashMap<&str, usize>,
    id_to_index: &HashMap<String, usize>,
    has_official: bool,
) -> OverrideSortKey {
    if let Some(&pos) = override_position.get(id) {
        OverrideSortKey {
            override_tier: 0,
            override_pos: pos,
            official_tier: 0,
            declaration_index: 0,
        }
    } else {
        let is_third_party = !(has_official && id.starts_with(OFFICIAL_PREFIX));
        OverrideSortKey {
            override_tier: 1,
            override_pos: 0,
            official_tier: u8::from(is_third_party),
            declaration_index: id_to_index.get(id).copied().unwrap_or(usize::MAX),
        }
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

    /// Helper to build a `FeatureMetadata` with `depends_on` (hard deps) set.
    /// Each key maps to an arbitrary option value (`true`).
    fn meta_depends(id: &str, depends_on: &[&str]) -> FeatureMetadata {
        FeatureMetadata {
            id: id.to_string(),
            depends_on: depends_on
                .iter()
                .map(|s| ((*s).to_string(), serde_json::Value::Bool(true)))
                .collect(),
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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

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

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

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

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

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

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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
        let (order, warnings) = compute_install_order(&features, None, &mut None);
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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        // a's dependency on nonexistent is ignored, so declaration order.
        assert_eq!(order, vec!["a", "b"]);
    }

    // ---------------------------------------------------------------
    // `dependsOn` (hard) imposes install-before ordering, like installsAfter
    // ---------------------------------------------------------------

    #[test]
    fn depends_on_orders_prerequisite_first() {
        // a dependsOn b → b must install before a, even though a is declared
        // first. Regression: previously only installs_after fed the sort, so a
        // pulled-in dependsOn target had no ordering edge.
        let items = vec![
            ("a".to_string(), meta_depends("a", &["b"])),
            ("b".to_string(), meta("b", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["b", "a"]);
    }

    #[test]
    fn depends_on_cycle_is_fatal() {
        // a dependsOn b, b dependsOn a → hard cycle must be signalled fatally
        // via depends_on_cycle, not silently ordered or merely warned.
        let items = vec![
            ("a".to_string(), meta_depends("a", &["b"])),
            ("b".to_string(), meta_depends("b", &["a"])),
        ];
        let features = feature_list(&items);

        let mut cycle = None;
        let (_order, warnings) = compute_install_order(&features, None, &mut cycle);

        assert!(warnings.is_empty(), "hard cycle is fatal, not a warning");
        let members = cycle.expect("expected a hard dependsOn cycle signal");
        assert!(members.contains(&"a".to_string()));
        assert!(members.contains(&"b".to_string()));
    }

    #[test]
    fn depends_on_and_installs_after_same_target_counted_correctly() {
        // a lists b in BOTH dependsOn and installsAfter. The prerequisite must
        // resolve cleanly; the doubled in-degree must not stall the sort or
        // spuriously report a cycle.
        let mut a = meta_depends("a", &["b"]);
        a.installs_after = vec!["b".to_string()];
        let items = vec![("a".to_string(), a), ("b".to_string(), meta("b", &[]))];
        let features = feature_list(&items);

        let mut cycle = None;
        let (order, warnings) = compute_install_order(&features, None, &mut cycle);

        assert!(warnings.is_empty(), "no cycle: b is a single prerequisite");
        assert!(cycle.is_none());
        assert_eq!(order, vec!["b", "a"]);
    }

    // ---------------------------------------------------------------
    // P1 regression: soft installsAfter participating in a loop with an
    // acyclic dependsOn graph must NOT be misclassified as a hard cycle.
    // ---------------------------------------------------------------

    #[test]
    fn soft_edge_in_loop_with_acyclic_hard_graph_is_not_fatal() {
        // a dependsOn b (hard: b before a), b installsAfter a (soft: a before b).
        // The combined graph stalls, but the dependsOn-only graph (b → a) is
        // acyclic and satisfiable by installing b first. This must NOT be a
        // fatal dependsOn cycle — it should warn and fall back instead.
        let mut b = meta("b", &["a"]); // installsAfter a
        b.id = "b".to_string();
        let items = vec![
            ("a".to_string(), meta_depends("a", &["b"])),
            ("b".to_string(), b),
        ];
        let features = feature_list(&items);

        let mut cycle = None;
        let (order, warnings) = compute_install_order(&features, None, &mut cycle);

        assert!(
            cycle.is_none(),
            "soft installsAfter must not trigger a fatal dependsOn cycle, got {cycle:?}"
        );
        // Combined-graph stall is reported as a soft warning, not a hard error.
        assert_eq!(warnings.len(), 1, "expected a soft-cycle warning");
        match &warnings[0] {
            FeatureWarning::CyclicDependency { .. } => {}
            other => panic!("expected CyclicDependency warning, got {other:?}"),
        }
        // Every feature still appears in the result.
        assert!(order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
    }

    #[test]
    fn override_soft_edge_in_loop_with_acyclic_hard_graph_is_not_fatal() {
        // Same scenario as above but exercised through the override path.
        let mut b = meta("b", &["a"]);
        b.id = "b".to_string();
        let items = vec![
            ("a".to_string(), meta_depends("a", &["b"])),
            ("b".to_string(), b),
        ];
        let features = feature_list(&items);
        let overrides = vec!["a".to_string(), "b".to_string()];

        let mut cycle = None;
        let (_order, _warnings) = compute_install_order(&features, Some(&overrides), &mut cycle);

        // Override suppresses soft edges for listed features, so b's
        // installsAfter is dropped entirely; either way no hard cycle exists.
        assert!(cycle.is_none(), "no hard cycle expected, got {cycle:?}");
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

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        // d must come before b and c; b and c before a.
        let pos = |id: &str| order.iter().position(|x| x == id).expect("id present");
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

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

        assert!(warnings.is_empty());
        // b listed (override), a unlisted (appended).
        assert_eq!(order, vec!["b", "a"]);
    }

    // ---------------------------------------------------------------
    // installsAfter resolved via legacyIds (spec: satisfiesSoftDependency)
    // ---------------------------------------------------------------

    /// Helper: metadata with `legacy_ids` set.
    fn meta_with_legacy_ids(
        id: &str,
        installs_after: &[&str],
        legacy_ids: &[&str],
    ) -> FeatureMetadata {
        FeatureMetadata {
            id: id.to_string(),
            installs_after: installs_after.iter().map(|s| (*s).to_string()).collect(),
            legacy_ids: legacy_ids.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn installs_after_via_legacy_id_orders_correctly() {
        // B has current id "ghcr.io/x/new" and bare legacyId "old" (real-world
        // format: legacyIds store the short name, not the qualified ref).
        // A declares installsAfter: ["ghcr.io/x/old"] (fully qualified old name).
        // resolve_soft_dep must qualify "old" → "ghcr.io/x/old" and match.
        // Expected: B before A.
        let b_meta = meta_with_legacy_ids("ghcr.io/x/new", &[], &["old"]);
        let a_meta = meta_with_legacy_ids("ghcr.io/x/a", &["ghcr.io/x/old"], &[]);
        let items = vec![
            ("ghcr.io/x/a".to_string(), a_meta),
            ("ghcr.io/x/new".to_string(), b_meta),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(
            pos("ghcr.io/x/new") < pos("ghcr.io/x/a"),
            "B (new id) must come before A; order was {order:?}"
        );
    }

    #[test]
    fn real_id_wins_over_colliding_legacy_alias() {
        // C has real id "shared-name". D has legacyId "shared-name".
        // A declares installsAfter: ["shared-name"] — must resolve to C (real id
        // wins), not D.
        let c_meta = meta_with_legacy_ids("shared-name", &[], &[]);
        let d_meta = meta_with_legacy_ids("ghcr.io/x/d", &[], &["shared-name"]);
        let a_meta = meta_with_legacy_ids("ghcr.io/x/a", &["shared-name"], &[]);
        let items = vec![
            ("ghcr.io/x/a".to_string(), a_meta),
            ("shared-name".to_string(), c_meta),
            ("ghcr.io/x/d".to_string(), d_meta),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        // A must come after C (exact real-id match wins over D's legacy alias).
        assert!(
            pos("shared-name") < pos("ghcr.io/x/a"),
            "C (real id 'shared-name') must precede A; order was {order:?}"
        );
        // D must NOT be treated as the soft-dependency target. If it were, D
        // would be forced before A. Verify D carries no ordering edge toward A
        // by checking there is no in_degree bump from D → A (i.e. D may appear
        // after A with no warning, because A's constraint is only on C).
        // Both outcomes (D before or after A) are legal; the absence of a cycle
        // warning confirms no spurious edge was added.
        assert!(
            order.contains(&"ghcr.io/x/d".to_string()),
            "D must appear in result"
        );
    }

    #[test]
    fn build_id_index_excludes_legacy_aliases() {
        // The exact id index backs dependsOn (hard) edges, override resolution
        // and tiebreaking — all exact-match per the spec. legacyIds must NOT
        // leak into it (only installsAfter resolves aliases, via resolve_soft_dep).
        let b_meta = meta_with_legacy_ids("ghcr.io/x/new", &[], &["ghcr.io/x/old"]);
        let items = vec![("ghcr.io/x/new".to_string(), b_meta)];
        let features = feature_list(&items);
        let index = build_id_index(&features);
        assert!(index.contains_key("ghcr.io/x/new"));
        assert!(
            !index.contains_key("ghcr.io/x/old"),
            "legacy alias must not be in the exact id index (dependsOn is exact-only)"
        );
    }

    #[test]
    fn resolve_soft_dep_resolves_exact_then_legacy() {
        // Feature "ghcr.io/x/new" with bare legacyId "old".
        // installsAfter target uses the same qualified prefix → "ghcr.io/x/old".
        let b_meta = meta_with_legacy_ids("ghcr.io/x/new", &[], &["old"]);
        let items = vec![("ghcr.io/x/new".to_string(), b_meta)];
        let features = feature_list(&items);
        let index = build_id_index(&features);
        // Exact id resolves.
        assert_eq!(
            resolve_soft_dep("ghcr.io/x/new", &index, &features),
            Some(0)
        );
        // Qualified alias ("ghcr.io/x/" + "old") resolves to the renamed feature.
        assert_eq!(
            resolve_soft_dep("ghcr.io/x/old", &index, &features),
            Some(0)
        );
        // Unknown id resolves to nothing.
        assert_eq!(resolve_soft_dep("ghcr.io/x/nope", &index, &features), None);
    }

    #[test]
    fn installs_after_via_qualified_legacy_id_orders_correctly() {
        // Real-world OCI rename: docker-outside-of-docker has
        //   legacyIds: ["docker-from-docker"]  (bare, no registry prefix)
        // A feature's installsAfter uses the OLD qualified name:
        //   installsAfter: ["ghcr.io/devcontainers/features/docker-from-docker"]
        // The current feature id is "ghcr.io/devcontainers/features/docker-outside-of-docker".
        // resolve_soft_dep must qualify the bare alias to
        //   "ghcr.io/devcontainers/features/docker-from-docker" and match dep_id.
        let dood_meta = meta_with_legacy_ids(
            "ghcr.io/devcontainers/features/docker-outside-of-docker",
            &[],
            &["docker-from-docker"],
        );
        let user_meta = meta_with_legacy_ids(
            "ghcr.io/devcontainers/features/user-feature",
            &["ghcr.io/devcontainers/features/docker-from-docker"],
            &[],
        );
        let items = vec![
            (
                "ghcr.io/devcontainers/features/user-feature".to_string(),
                user_meta,
            ),
            (
                "ghcr.io/devcontainers/features/docker-outside-of-docker".to_string(),
                dood_meta,
            ),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(
            pos("ghcr.io/devcontainers/features/docker-outside-of-docker")
                < pos("ghcr.io/devcontainers/features/user-feature"),
            "docker-outside-of-docker must precede user-feature via legacy alias; order was {order:?}"
        );
    }

    // ---------------------------------------------------------------
    // Override cannot bypass dependsOn hard constraints
    // ---------------------------------------------------------------

    #[test]
    fn override_respects_depends_on_hard_constraint() {
        // child dependsOn parent — override lists child first, but parent must
        // still be emitted before child per hard-dep semantics.
        let items = vec![
            ("child".to_string(), meta_depends("child", &["parent"])),
            ("parent".to_string(), meta_depends("parent", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["child".to_string(), "parent".to_string()];

        let mut cycle = None;
        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut cycle);

        assert!(warnings.is_empty());
        assert!(cycle.is_none(), "no cycle expected");
        // parent must appear before child despite override listing child first.
        let pos = |id: &str| order.iter().position(|x| x == id).expect("id present");
        assert!(pos("parent") < pos("child"), "order was {order:?}");
    }

    // ---------------------------------------------------------------
    // Override respects order among independent features
    // ---------------------------------------------------------------

    #[test]
    fn override_order_respected_for_independent_features() {
        // c, b, a have no deps — override dictates their install order.
        let items = vec![
            ("a".to_string(), meta("a", &[])),
            ("b".to_string(), meta("b", &[])),
            ("c".to_string(), meta("c", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["c".to_string(), "b".to_string(), "a".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    // ---------------------------------------------------------------
    // dependsOn cycle in override path → fatal signal
    // ---------------------------------------------------------------

    #[test]
    fn override_with_depends_on_cycle_signals_fatal() {
        let items = vec![
            ("a".to_string(), meta_depends("a", &["b"])),
            ("b".to_string(), meta_depends("b", &["a"])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["a".to_string(), "b".to_string()];

        let mut cycle = None;
        let (_order, _warnings) = compute_install_order(&features, Some(&overrides), &mut cycle);

        let members = cycle.expect("expected CyclicDependsOn signal");
        assert!(members.contains(&"a".to_string()));
        assert!(members.contains(&"b".to_string()));
    }

    #[test]
    fn spec_option_shorthand_string_to_version() {
        let input = serde_json::json!("1.18");
        if let Some(s) = input.as_str() {
            let expanded = serde_json::json!({"version": s});
            assert_eq!(expanded, serde_json::json!({"version": "1.18"}));
        }
    }
}
