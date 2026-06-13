//! Dependency ordering for devcontainer features.
//!
//! Implements topological sort (Kahn's algorithm) over both `dependsOn` (hard)
//! and `installsAfter` (soft) dependencies — every prerequisite must install
//! first — with tiebreaking rules matching the devcontainer spec's "Round
//! Stable Sort":
//!
//! 1. Same-level features (no ordering constraint between them) sort
//!    lexicographically by their canonical resource name — the feature
//!    reference with any `:tag`/`@digest` version stripped — mirroring
//!    `compareTo` -> `ociResourceCompareTo` in the official CLI's
//!    `containerFeaturesOrder.ts`. There is no "official features first"
//!    heuristic; `ghcr.io/devcontainers/...` only wins ties when it sorts
//!    earlier alphabetically.
//! 2. Equal canonical names break the tie by version tag (lexicographically),
//!    then by declaration order for byte-identical references.
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

use std::collections::{HashMap, HashSet};

use crate::error::FeatureWarning;
use crate::types::FeatureMetadata;

/// Split a feature reference into its canonical resource name and version.
///
/// Mirrors the official `ociResourceCompareTo`, which sorts on the resource
/// (`registry/namespace/id`) first and the tag only as a secondary key. We
/// strip a trailing `@digest` first, then a trailing `:tag`, but never touch
/// the `:` inside a `host:port` registry — a version separator only appears
/// after the final path segment, so we look past the last `/`.
fn split_ref_version(id: &str) -> (&str, &str) {
    // A digest (`@…`) always wins over a tag, and may itself contain a `:`
    // (`@sha256:…`), so look for it first and split there.
    if let Some(at) = id.rfind('@') {
        return id.split_at(at);
    }
    // Otherwise a `:tag` separator only appears after the final path segment —
    // never inside a `host:port` registry, which lives before the first `/`.
    let last_segment_start = id.rfind('/').map_or(0, |slash| slash + 1);
    id[last_segment_start..]
        .find(':')
        .map_or((id, ""), |rel| id.split_at(last_segment_start + rel))
    // The returned version keeps its leading separator (`:1` / `@sha256:…`);
    // that's fine for a stable secondary sort.
}

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

/// Tiebreak key for same-level features within an install round.
///
/// Matches the official CLI's `compareTo` -> `ociResourceCompareTo`: sort by
/// canonical resource name lexicographically, then by version tag, then by
/// declaration order for byte-identical references. Smaller key = installed
/// first. Borrows the feature reference from the input slice, so building a key
/// is allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey<'a> {
    /// Canonical resource name (the feature reference without its version).
    resource: &'a str,
    /// Version tag/digest (with its leading separator), as a secondary key.
    version: &'a str,
    /// Index in the original declaration order — stable fallback for
    /// byte-identical references.
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
/// lexicographic same-level tiebreaking, and any explicit override.
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

    // Handle override ordering.
    if let Some(overrides) = override_order {
        return apply_override(
            features,
            overrides,
            &id_to_index,
            depends_on_cycle,
            &mut warnings,
        );
    }

    // No override — run full topological sort.
    topological_sort(features, &id_to_index, depends_on_cycle, &mut warnings)
}

/// Apply `overrideFeatureInstallOrder` as a per-round **priority hint**.
///
/// Per the devcontainer spec, `overrideFeatureInstallOrder` raises the round
/// priority of the listed features so they are committed in earlier rounds. It
/// does NOT bypass `dependsOn` hard constraints — a feature that `dependsOn`
/// another must still wait for that prerequisite regardless of its position in
/// the override list. Mirrors `applyOverrideFeatureInstallOrder`: the first
/// override entry gets the highest priority (`len`), the last gets `1`, and
/// unlisted features keep priority `0`.
///
/// Listed features also have their own `installsAfter` soft edges ignored so
/// the override order wins among them.
fn apply_override(
    features: &[(String, &FeatureMetadata)],
    overrides: &[String],
    id_to_index: &HashMap<String, usize>,
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

    // Round priority: first override entry installs soonest (highest priority).
    let len = overrides.len();
    let round_priority: Vec<usize> = features
        .iter()
        .map(|(id, _)| {
            override_position
                .get(id.as_str())
                .map_or(0, |&pos| len - pos)
        })
        .collect();

    let result = schedule_rounds(features, &graph, id_to_index, &round_priority);
    finish_schedule(features, &graph, result, depends_on_cycle, warnings)
}

/// Topological sort with the spec's "Round Stable Sort" tiebreaking and no
/// override priorities (every feature shares round priority `0`).
fn topological_sort(
    features: &[(String, &FeatureMetadata)],
    id_to_index: &HashMap<String, usize>,
    depends_on_cycle: &mut Option<Vec<String>>,
    warnings: &mut Vec<FeatureWarning>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    let graph = build_graph(features, id_to_index, None);
    let round_priority = vec![0; features.len()];
    let result = schedule_rounds(features, &graph, id_to_index, &round_priority);
    finish_schedule(features, &graph, result, depends_on_cycle, warnings)
}

/// Round-based scheduler matching the official `computeDependsOnInstallationOrder`.
///
/// Each round collects **every** currently-eligible feature (all hard and soft
/// prerequisites already committed in prior rounds), keeps only those at the
/// highest round priority present, sorts that batch by [`SortKey`], and commits
/// the whole batch before any feature it unblocks becomes eligible. Committing
/// per-round — rather than per-node from a global priority queue — is what
/// preserves the spec's round boundaries: a dependent unblocked this round
/// cannot leap ahead of features that were already eligible at the round start.
///
/// Returns the committed feature ids in install order. A short result (fewer
/// than `features.len()`) signals a stall for the caller's cycle detection.
fn schedule_rounds(
    features: &[(String, &FeatureMetadata)],
    graph: &DepGraph,
    original_id_to_index: &HashMap<String, usize>,
    round_priority: &[usize],
) -> Vec<String> {
    let n = features.len();
    let mut in_degree = graph.in_degree.clone();
    let mut committed = vec![false; n];
    let mut result = Vec::with_capacity(n);

    loop {
        // Eligible = not yet committed and all prerequisites already committed.
        let eligible: Vec<usize> = (0..n)
            .filter(|&idx| !committed[idx] && in_degree[idx] == 0)
            .collect();
        if eligible.is_empty() {
            break;
        }

        // Honor round priority: only commit the highest-priority eligible nodes
        // this round (overrideFeatureInstallOrder raises listed features).
        let max_priority = eligible
            .iter()
            .map(|&idx| round_priority[idx])
            .max()
            .unwrap_or(0);
        let mut round: Vec<usize> = eligible
            .into_iter()
            .filter(|&idx| round_priority[idx] == max_priority)
            .collect();

        // Sort the round lexicographically (the spec's stable tiebreak).
        round.sort_by(|&a, &b| {
            sort_key(&features[a].0, original_id_to_index)
                .cmp(&sort_key(&features[b].0, original_id_to_index))
        });

        // Commit the whole round, then unblock dependents for the next round.
        for &idx in &round {
            committed[idx] = true;
            result.push(features[idx].0.clone());
        }
        for &idx in &round {
            for &dependent in graph.hard_adj[idx].iter().chain(&graph.soft_adj[idx]) {
                in_degree[dependent] -= 1;
            }
        }
    }

    result
}

/// Finalize a scheduled order: append the warnings/fallback from cycle
/// detection when the scheduler stalled before placing every feature.
fn finish_schedule(
    features: &[(String, &FeatureMetadata)],
    graph: &DepGraph,
    mut result: Vec<String>,
    depends_on_cycle: &mut Option<Vec<String>>,
    warnings: &mut Vec<FeatureWarning>,
) -> (Vec<String>, Vec<FeatureWarning>) {
    if let Some(fallback) =
        finish_with_cycle_detection(features, graph, &result, depends_on_cycle, warnings)
    {
        result.extend(fallback);
    }
    (result, warnings.clone())
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

/// Compute the lexicographic tiebreak key for a feature id.
///
/// Matches the official CLI's `compareTo` -> `ociResourceCompareTo`: sort by
/// canonical resource name first, then by version tag, then by declaration
/// order as a stable fallback for byte-identical references. Borrows from `id`,
/// so the returned key lives as long as the reference it was built from.
fn sort_key<'a>(id: &'a str, original_id_to_index: &HashMap<String, usize>) -> SortKey<'a> {
    let (resource, version) = split_ref_version(id);
    let declaration_index = original_id_to_index.get(id).copied().unwrap_or(usize::MAX);
    SortKey {
        resource,
        version,
        declaration_index,
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
    // No dependencies → lexicographic order (here == declaration order)
    // ---------------------------------------------------------------

    #[test]
    fn no_dependencies_sorts_lexicographically() {
        let items = vec![
            ("alpha".to_string(), meta("alpha", &[])),
            ("beta".to_string(), meta("beta", &[])),
            ("gamma".to_string(), meta("gamma", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        // alpha < beta < gamma — lexicographic order, which here coincides
        // with declaration order.
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
    // Lexicographic tiebreak: official id sorts before third-party here
    // ---------------------------------------------------------------

    #[test]
    fn official_sorts_before_third_party_lexicographically() {
        let official_id = "ghcr.io/devcontainers/features/node";
        let third_party = "ghcr.io/someuser/features/foo";

        let items = vec![
            (third_party.to_string(), meta(third_party, &[])),
            (official_id.to_string(), meta(official_id, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        // "ghcr.io/devcontainers/..." < "ghcr.io/someuser/..." lexicographically
        // ('d' < 's'), so official sorts first despite being declared second —
        // no official-first tier, just lex order.
        assert_eq!(order, vec![official_id, third_party]);
    }

    // ---------------------------------------------------------------
    // All third-party → lexicographic by id (here == declaration order)
    // ---------------------------------------------------------------

    #[test]
    fn all_third_party_sorted_lexicographically() {
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
    // Lexicographic tiebreak with dependencies
    // ---------------------------------------------------------------

    #[test]
    fn lexicographic_tiebreak_with_dependencies() {
        let official = "ghcr.io/devcontainers/features/node";
        let third = "ghcr.io/someuser/features/tool";
        let another = "ghcr.io/anotheruser/features/util";

        // third installsAfter another, official has no deps.
        let items = vec![
            (third.to_string(), meta(third, &[another])),
            (official.to_string(), meta(official, &[])),
            (another.to_string(), meta(another, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        // `another` and `official` are both unconstrained at the start;
        // "ghcr.io/anotheruser/..." < "ghcr.io/devcontainers/..." ('a' < 'd'),
        // so `another` emits first, then `official`, then `third`
        // (installsAfter another). No official-first tier.
        assert_eq!(order, vec![another, official, third]);
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

    // ---------------------------------------------------------------
    // Lexicographic tiebreak: independent features declared out of lex order
    // ---------------------------------------------------------------

    #[test]
    fn independent_features_sorted_lexicographically() {
        // Declared in reverse lex order — install order must be alphabetical.
        let items = vec![
            ("ghcr.io/x/zebra".to_string(), meta("ghcr.io/x/zebra", &[])),
            ("ghcr.io/x/alpha".to_string(), meta("ghcr.io/x/alpha", &[])),
            ("ghcr.io/x/mango".to_string(), meta("ghcr.io/x/mango", &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        // Lexicographic order: alpha < mango < zebra (not declaration order).
        assert_eq!(
            order,
            vec!["ghcr.io/x/alpha", "ghcr.io/x/mango", "ghcr.io/x/zebra"]
        );
    }

    // ---------------------------------------------------------------
    // Lexicographic tiebreak: third-party before official when lex says so
    // ---------------------------------------------------------------

    #[test]
    fn third_party_before_official_when_lex_order_demands() {
        // "ghcr.io/aaa-corp/..." < "ghcr.io/devcontainers/..." ('a' < 'd'),
        // so the third-party feature must install BEFORE the official one —
        // proving there is no official-first tier, only lexicographic order.
        let official = "ghcr.io/devcontainers/features/node";
        let third_party = "ghcr.io/aaa-corp/features/tool";

        let items = vec![
            (official.to_string(), meta(official, &[])),
            (third_party.to_string(), meta(third_party, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec![third_party, official]);
    }

    // ---------------------------------------------------------------
    // Tiebreak primary key is the canonical resource (version ignored first)
    // ---------------------------------------------------------------

    #[test]
    fn tiebreak_sorts_by_resource_before_version() {
        // `node:2` shares the `node` resource; `python:1` is a different
        // resource. Resource name is the primary key, so node:2 (resource
        // "...node") sorts before python:1 ("...python") even though '2' > '1'.
        let node = "ghcr.io/devcontainers/features/node:2";
        let python = "ghcr.io/devcontainers/features/python:1";

        let items = vec![
            (python.to_string(), meta(python, &[])),
            (node.to_string(), meta(node, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec![node, python]);
    }

    // ---------------------------------------------------------------
    // Version tag is the secondary tiebreak for the same resource
    // ---------------------------------------------------------------

    #[test]
    fn tiebreak_falls_back_to_version_for_same_resource() {
        // Same resource, differing tags — secondary key (version) orders them
        // lexicographically: ":1" < ":2".
        let v2 = "ghcr.io/devcontainers/features/node:2";
        let v1 = "ghcr.io/devcontainers/features/node:1";

        let items = vec![
            (v2.to_string(), meta(v2, &[])),
            (v1.to_string(), meta(v1, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec![v1, v2]);
    }

    // ---------------------------------------------------------------
    // Tiebreak compares canonical resource, not the raw ref with its tag
    // ---------------------------------------------------------------

    #[test]
    fn tiebreak_compares_resource_not_raw_ref_with_tag() {
        // Raw-ref comparison would order `go-tools:1` before `go:1` because
        // '-' (0x2d) < ':' (0x3a). Comparing the canonical resource instead
        // puts `go` (shorter prefix) before `go-tools`, matching the official.
        let go = "ghcr.io/devcontainers/features/go:1";
        let go_tools = "ghcr.io/devcontainers/features/go-tools:1";

        let items = vec![
            (go_tools.to_string(), meta(go_tools, &[])),
            (go.to_string(), meta(go, &[])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec![go, go_tools]);
    }

    // ---------------------------------------------------------------
    // Round boundaries: a dependent unblocked mid-schedule waits a round
    // ---------------------------------------------------------------

    #[test]
    fn unblocked_dependent_does_not_leap_eligible_features() {
        // `b` and `z` are eligible in round 1; `a` installsAfter `b`.
        // Round 1 commits {b, z} (sorted: b < z); only then does `a` become
        // eligible (round 2). Result must be b, z, a — NOT b, a, z, which a
        // global priority queue would produce (popping b unblocks a, and
        // a < z). This is the official round-stable behavior.
        let items = vec![
            ("b".to_string(), meta("b", &[])),
            ("z".to_string(), meta("z", &[])),
            ("a".to_string(), meta("a", &["b"])),
        ];
        let features = feature_list(&items);

        let (order, warnings) = compute_install_order(&features, None, &mut None);

        assert!(warnings.is_empty());
        assert_eq!(order, vec!["b", "z", "a"]);
    }

    // ---------------------------------------------------------------
    // Override priority installs listed features in earlier rounds
    // ---------------------------------------------------------------

    #[test]
    fn override_priority_pulls_listed_feature_into_earlier_round() {
        // All three are independent. Override lists only `z`, so `z` gets the
        // highest round priority and is committed alone in round 1, ahead of
        // `a` and `b` (which would otherwise sort first lexicographically).
        let items = vec![
            ("a".to_string(), meta("a", &[])),
            ("b".to_string(), meta("b", &[])),
            ("z".to_string(), meta("z", &[])),
        ];
        let features = feature_list(&items);
        let overrides = vec!["z".to_string()];

        let (order, warnings) = compute_install_order(&features, Some(&overrides), &mut None);

        assert!(warnings.is_empty());
        // z (override priority) first, then a, b (lex order) in the next round.
        assert_eq!(order, vec!["z", "a", "b"]);
    }

    // ---------------------------------------------------------------
    // split_ref_version: tag/digest stripping, host:port left intact
    // ---------------------------------------------------------------

    #[test]
    fn split_ref_version_handles_tag_digest_and_port() {
        // Plain ref, no version.
        assert_eq!(
            split_ref_version("ghcr.io/devcontainers/features/node"),
            ("ghcr.io/devcontainers/features/node", "")
        );
        // Tagged ref splits at the final-segment colon.
        assert_eq!(
            split_ref_version("ghcr.io/devcontainers/features/node:1"),
            ("ghcr.io/devcontainers/features/node", ":1")
        );
        // Digest splits at '@' and keeps the digest as the version.
        assert_eq!(
            split_ref_version("ghcr.io/x/node@sha256:abc"),
            ("ghcr.io/x/node", "@sha256:abc")
        );
        // host:port in the registry must NOT be mistaken for a version.
        assert_eq!(
            split_ref_version("localhost:5000/x/node"),
            ("localhost:5000/x/node", "")
        );
        assert_eq!(
            split_ref_version("localhost:5000/x/node:1"),
            ("localhost:5000/x/node", ":1")
        );
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
