//! Structure and ordering of OCI image tags.
//!
//! Registries return `/v2/<name>/tags/list` in lexical order, which puts
//! `2.0.9-trixie` after `2.0.14-trixie`. Everything here exists so callers
//! rank tags by version instead of by string.

use std::collections::BTreeSet;

use tracing::warn;

use crate::tag_cache::TagCache;

/// Maximum number of pinned tags to present to the user.
pub const MAX_PINNED_TAGS: usize = 15;

/// A repository's vocabulary of variants, learned from its published tags.
///
/// OCI tag shapes are repository-specific: the same numeric-looking prefix
/// can be an image version in one repository and part of a floating variant
/// in another. Keeping the vocabulary makes that distinction explicit.
#[derive(Debug, Clone)]
pub struct TagGrammar {
    variants: BTreeSet<String>,
}

/// A tag interpreted using the variants published by its repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedTag<'a> {
    /// `None` means the tag is floating: it names a variant with no image version.
    pub version: Option<&'a str>,
    /// The repository-defined variant, or an empty string for a numeric-only tag.
    pub variant: &'a str,
}

impl TagGrammar {
    /// Learn the variant vocabulary represented by a complete published tag list.
    ///
    /// A numeric version followed by a non-empty suffix is the only unambiguous
    /// evidence that the suffix names a variant.
    #[must_use]
    pub fn from_tags(tags: &[&str]) -> Self {
        let variants = tags
            .iter()
            .filter_map(|tag| {
                let (version, variant) = tag.split_once('-')?;
                (is_numeric_group(version) && !variant.is_empty()).then(|| variant.to_owned())
            })
            .collect();
        Self { variants }
    }

    /// Interpret a tag using the repository's learned variant vocabulary.
    ///
    /// The longest published variant wins so composite variants remain intact,
    /// and parsing borrows slices from `tag` without allocating.
    #[must_use]
    pub fn parse<'a>(&self, tag: &'a str) -> Option<ParsedTag<'a>> {
        if self.variants.contains(tag) {
            return Some(ParsedTag {
                version: None,
                variant: tag,
            });
        }

        for (dash, _) in tag.match_indices('-') {
            let version = &tag[..dash];
            let variant = &tag[dash + 1..];
            if self.variants.contains(variant) {
                return is_numeric_group(version).then_some(ParsedTag {
                    version: Some(version),
                    variant,
                });
            }
        }

        is_numeric_group(tag).then_some(ParsedTag {
            version: Some(tag),
            variant: "",
        })
    }
}

/// Select the tags that pin a grammar-recognized selection to a more specific
/// image version, newest first.
///
/// A floating selection accepts versioned tags with the same variant. A
/// versioned selection accepts tags with the same variant whose version adds
/// dotted segments to the selected version. An unknown selection accepts
/// nothing, because suffix overlap alone is not evidence of variant identity.
///
/// The selection itself has no added version specificity and is never
/// offered. Tags are sorted by their version part descending, with more specific
/// versions ranking above their aliases (`4.0.10` > `4.0` > `4`), and
/// capped at [`MAX_PINNED_TAGS`].
pub fn pinnable_tags<'a>(tags: &[&'a str], selection: &str) -> Vec<&'a str> {
    let grammar = TagGrammar::from_tags(tags);
    let Some(selection) = grammar.parse(selection) else {
        return Vec::new();
    };
    let mut keyed: Vec<(VersionKey, &'a str)> = tags
        .iter()
        .filter_map(|&tag| Some((pin_version_key(&grammar, tag, selection)?, tag)))
        .collect();
    keyed.sort_unstable_by(|a, b| b.cmp(a));
    keyed.truncate(MAX_PINNED_TAGS);
    keyed.into_iter().map(|(_, tag)| tag).collect()
}

/// Compute the sort key when `tag` refines the parsed `selection` under the
/// repository grammar.
fn pin_version_key(
    grammar: &TagGrammar,
    tag: &str,
    selection: ParsedTag<'_>,
) -> Option<VersionKey> {
    let parsed = grammar.parse(tag)?;
    if parsed.variant != selection.variant {
        return None;
    }

    let version = parsed.version?;
    if let Some(selected_version) = selection.version {
        version.strip_prefix(selected_version)?.strip_prefix('.')?;
    }
    version_key(version)
}

/// Split a tag into its leading numeric version and its trailing variant.
///
/// The version is the longest run of leading dash-separated groups whose
/// segments are all decimal digits; everything after it is the variant.
///
/// Returns `None` when there is no leading numeric group at all, which is
/// exactly the set of floating tags (`latest`, `trixie`, `dev-1-trixie`).
pub fn split_tag(tag: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for group in tag.split('-') {
        if !is_numeric_group(group) {
            break;
        }
        end += group.len() + 1;
    }
    if end == 0 {
        return None;
    }
    Some((&tag[..end - 1], tag.get(end..).unwrap_or("")))
}

fn is_numeric_group(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|segment| !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Sort key for a tag's version part.
///
/// Dash-separated groups of dot-separated numbers, compared group by group
/// so that a dotted refinement outranks its alias within the same group
/// (`4.0.10` > `4.0` > `4`) regardless of what follows a dash.
pub type VersionKey = Vec<Vec<u64>>;

/// Parse a version prefix like `"4.0.10"` or `"4.0.10-24"` into its
/// numeric sort key.
///
/// Returns `None` unless every segment is numeric.
pub fn version_key(prefix: &str) -> Option<VersionKey> {
    prefix
        .split('-')
        .map(|group| {
            group
                .split('.')
                .map(|segment| segment.parse::<u64>().ok())
                .collect()
        })
        .collect()
}

/// Where a tag list came from.
///
/// Callers surface [`Self::StaleCache`] to the user: the answer is real but
/// may be out of date, which is a different thing from a fresh answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSource {
    /// Fetched from the registry just now.
    Registry,
    /// Served from a cache entry still inside its TTL.
    Cache,
    /// Served from an expired cache entry because the registry was
    /// unreachable.
    StaleCache,
}

/// A published tag list together with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedTags {
    /// The tags, in registry order (lexical — rank them before use).
    pub tags: Vec<String>,
    /// Provenance of `tags`.
    pub source: TagSource,
}

/// Fetch the published tag list for an image reference, via a 1-hour cache.
///
/// When the registry cannot be reached, an expired cache entry is served
/// instead and flagged as [`TagSource::StaleCache`] so the caller can warn.
///
/// # Errors
///
/// Returns an error when the registry cannot be reached and no cached tag
/// list exists for this reference.
pub async fn fetch_image_tags(
    cache: &TagCache,
    reference: &str,
    force_refresh: bool,
) -> miette::Result<FetchedTags> {
    // Normalize here rather than in `parse_reference`: `ubuntu:24.04` is a
    // legitimate image pin, but the same shorthand in a feature reference is
    // a typo worth reporting. Done before the cache lookup so both spellings
    // of a reference share one cache entry.
    let reference = &crate::normalize_reference(reference);

    if !force_refresh && let Some(tags) = cache.get(reference) {
        return Ok(FetchedTags {
            tags,
            source: TagSource::Cache,
        });
    }

    match crate::fetch_published_tags(reference).await {
        Ok(tags) => {
            let _ = cache.put(reference, &tags);
            Ok(FetchedTags {
                tags,
                source: TagSource::Registry,
            })
        }
        Err(e) => cache.get_stale(reference).map_or(Err(e), |tags| {
            warn!("registry unreachable for {reference}; using cached tags");
            Ok(FetchedTags {
                tags,
                source: TagSource::StaleCache,
            })
        }),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const TYPESCRIPT_NODE_SHAPED_TAGS: [&str; 6] = [
        "5.0.3-24-trixie",
        "24-trixie",
        "5.0.3-trixie",
        "trixie",
        "dev-24-trixie",
        "latest",
    ];

    fn typescript_node_grammar() -> TagGrammar {
        TagGrammar::from_tags(&TYPESCRIPT_NODE_SHAPED_TAGS)
    }

    fn fixture_tags(raw: &str) -> Vec<String> {
        serde_json::from_str::<serde_json::Value>(raw).unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn splits_version_from_variant() {
        assert_eq!(split_tag("2.0.14-trixie"), Some(("2.0.14", "trixie")));
        assert_eq!(split_tag("2.0.9-1-trixie"), Some(("2.0.9-1", "trixie")));
        assert_eq!(split_tag("1-ubuntu-24.04"), Some(("1", "ubuntu-24.04")));
        assert_eq!(split_tag("24.04"), Some(("24.04", "")));
    }

    #[test]
    fn rejects_tags_without_a_leading_version() {
        assert_eq!(split_tag("latest"), None);
        assert_eq!(split_tag("trixie"), None);
        assert_eq!(split_tag("dev-1-trixie"), None);
        assert_eq!(split_tag("bookworm"), None);
    }

    #[test]
    fn grammar_parses_versioned_tag() {
        assert_eq!(
            typescript_node_grammar().parse("5.0.3-trixie"),
            Some(ParsedTag {
                version: Some("5.0.3"),
                variant: "trixie",
            })
        );
    }

    #[test]
    fn grammar_parses_composite_variant_as_floating() {
        assert_eq!(
            typescript_node_grammar().parse("24-trixie"),
            Some(ParsedTag {
                version: None,
                variant: "24-trixie",
            })
        );
    }

    #[test]
    fn grammar_parses_versioned_composite_variant() {
        assert_eq!(
            typescript_node_grammar().parse("5.0.3-24-trixie"),
            Some(ParsedTag {
                version: Some("5.0.3"),
                variant: "24-trixie",
            })
        );
    }

    #[test]
    fn grammar_parses_plain_variant_as_floating() {
        assert_eq!(
            typescript_node_grammar().parse("trixie"),
            Some(ParsedTag {
                version: None,
                variant: "trixie",
            })
        );
    }

    #[test]
    fn grammar_rejects_non_numeric_version_prefix() {
        assert_eq!(typescript_node_grammar().parse("dev-24-trixie"), None);
    }

    #[test]
    fn grammar_rejects_unknown_floating_tag() {
        assert_eq!(typescript_node_grammar().parse("latest"), None);
    }

    #[test]
    fn grammar_parses_numeric_tag_without_variant() {
        assert_eq!(
            typescript_node_grammar().parse("24.04"),
            Some(ParsedTag {
                version: Some("24.04"),
                variant: "",
            })
        );
    }

    #[test]
    fn grammar_preserves_rust_revision_variants() {
        let tags = fixture_tags(include_str!(
            "../../cella-cli/testdata/mcr-devcontainers-rust-tags.json"
        ));
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let grammar = TagGrammar::from_tags(&refs);

        assert_eq!(
            grammar.parse("2.0.14-1-trixie"),
            Some(ParsedTag {
                version: Some("2.0.14"),
                variant: "1-trixie",
            })
        );
        assert_eq!(
            grammar.parse("2.0.14-trixie"),
            Some(ParsedTag {
                version: Some("2.0.14"),
                variant: "trixie",
            })
        );
    }

    #[test]
    fn grammar_derives_expected_base_variants() {
        let tags = fixture_tags(include_str!("../testdata/mcr-devcontainers-base-tags.json"));
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();

        assert_eq!(tags.len(), 1_924);
        assert_eq!(TagGrammar::from_tags(&refs).variants.len(), 54);
    }

    #[test]
    fn grammar_derives_expected_typescript_node_variants() {
        let tags = fixture_tags(include_str!(
            "../../cella-cli/testdata/mcr-devcontainers-typescript-node-tags.json"
        ));
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();

        assert_eq!(tags.len(), 942);
        assert_eq!(TagGrammar::from_tags(&refs).variants.len(), 28);
    }

    #[test]
    fn ranks_numerically_not_lexically() {
        let tags = ["2.0.9-trixie", "2.0.14-trixie", "2.0.2-trixie"];
        let ranked = pinnable_tags(&tags, "trixie");
        assert_eq!(ranked.first(), Some(&"2.0.14-trixie"));
    }

    #[test]
    fn excludes_floating_and_other_variants() {
        let tags = ["2.0.14-trixie", "trixie", "dev-1-trixie", "2.0.14-bookworm"];
        let ranked = pinnable_tags(&tags, "trixie");
        assert_eq!(ranked, vec!["2.0.14-trixie"]);
    }

    #[test]
    fn pinnable_tags_composite_selection_offers_only_matching_variant() {
        // Regression: selecting "24-trixie" must offer version-prefixed
        // refinements of exactly that variant — not other node versions,
        // not dev builds. Tag list taken verbatim from
        // mcr.microsoft.com/devcontainers/typescript-node.
        let tags = vec![
            "dev-24-trixie",
            "5.0.1-24-trixie",
            "24-trixie",
            "5-24-trixie",
            "5.0-24-trixie",
            "4.0.10-24-trixie",
            "4-24-trixie",
            "4.0-24-trixie",
            "4.0.9-24-trixie",
            "dev-trixie",
            "22-trixie",
            "5.0.1-22-trixie",
            "trixie",
            "4.0.10-trixie",
            "latest",
        ];
        let pinned = pinnable_tags(&tags, "24-trixie");
        assert_eq!(
            pinned,
            vec![
                "5.0.1-24-trixie",
                "5.0-24-trixie",
                "5-24-trixie",
                "4.0.10-24-trixie",
                "4.0.9-24-trixie",
                "4.0-24-trixie",
                "4-24-trixie",
            ]
        );
    }

    #[test]
    fn pinnable_tags_specific_versions_rank_above_aliases_in_composite_tags() {
        // Regression: the flat numeric key ranked "4-24-trixie" ([4, 24])
        // above "4.0.10-24-trixie" ([4, 0, 10, 24]) because 24 > 0 at
        // index 1. Dash groups must be compared before dot segments. This list
        // derives only {"24-trixie"} as its variant vocabulary, so "trixie" is
        // not a variant here and cannot select these tags.
        let tags = vec!["4-24-trixie", "4.0-24-trixie", "4.0.10-24-trixie"];
        assert_eq!(
            pinnable_tags(&tags, "24-trixie"),
            vec!["4.0.10-24-trixie", "4.0-24-trixie", "4-24-trixie"]
        );
    }

    #[test]
    fn pinnable_tags_typescript_node_codename_stays_on_its_variant() {
        let tags = fixture_tags(include_str!(
            "../../cella-cli/testdata/mcr-devcontainers-typescript-node-tags.json"
        ));
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let pinned = pinnable_tags(&refs, "trixie");

        assert!(pinned.contains(&"5.0.3-trixie"));
        for alias in ["24-trixie", "22-trixie", "20-trixie"] {
            assert!(!pinned.contains(&alias), "offered node alias {alias}");
        }
    }

    #[test]
    fn pinnable_tags_codename_selection() {
        let tags = vec![
            "1.0.9-trixie",
            "1-trixie",
            "dev-trixie",
            "trixie",
            "1.0.9-bookworm",
            "latest",
        ];
        assert_eq!(
            pinnable_tags(&tags, "trixie"),
            vec!["1.0.9-trixie", "1-trixie"]
        );
    }

    #[test]
    fn pinnable_tags_offers_extensions_of_the_selection_version() {
        // Node-style registries refine "24-trixie" as "24.7.0-trixie"
        // instead of prefixing an image version. These must be offered,
        // still excluding other variants and codenames.
        let tags = vec![
            "24.7.0-trixie",
            "24.6.1-trixie",
            "24-trixie",
            "22.1.0-trixie",
            "24.7.0-bookworm",
            "dev-trixie",
            "latest",
        ];
        assert_eq!(
            pinnable_tags(&tags, "24-trixie"),
            vec!["24.7.0-trixie", "24.6.1-trixie"]
        );
    }

    #[test]
    fn pinnable_tags_numeric_only_selection_extension() {
        let tags = vec!["22.12.0", "22.11.0", "22", "20.9.0", "latest"];
        assert_eq!(pinnable_tags(&tags, "22"), vec!["22.12.0", "22.11.0"]);
    }

    #[test]
    fn pinnable_tags_excludes_selection_and_non_numeric_prefixes() {
        let tags = vec!["24-trixie", "dev-24-trixie", "-24-trixie", "rc1-24-trixie"];
        assert!(pinnable_tags(&tags, "24-trixie").is_empty());
    }

    #[test]
    fn pinnable_tags_no_matches() {
        let tags = vec!["latest", "bookworm", "1.2.3-bookworm"];
        assert!(pinnable_tags(&tags, "trixie").is_empty());
    }

    /// With nothing cached there is no answer to give, so the failure has to
    /// be actionable on its own.
    #[tokio::test]
    async fn unreachable_registry_without_cache_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());

        let err = fetch_image_tags(&cache, "registry.invalid/foo/bar", true)
            .await
            .expect_err("an unresolvable registry must not succeed");
        let rendered = format!("{err:?}");

        assert!(
            rendered.contains("registry.invalid"),
            "diagnostic must name the reference, got: {rendered}"
        );

        // Assert on the structured help rather than `rendered` — miette
        // word-wraps to terminal width and would split the path.
        let help = miette::Diagnostic::help(&*err)
            .expect("diagnostic must tell the user what to check")
            .to_string();
        assert!(
            help.contains("~/.docker/config.json"),
            "help must point at the credential store, got: {help}"
        );
    }

    /// The offline path: an unreachable registry must serve whatever the
    /// cache holds rather than failing, and must say the answer is stale.
    #[tokio::test]
    async fn unreachable_registry_falls_back_to_stale_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        let cached = vec!["2.0.14-trixie".to_owned()];
        cache.put("registry.invalid/foo/bar", &cached).unwrap();

        // Force a refresh so the fresh-cache branch cannot serve this, then
        // backdate so only the stale reader can.
        let old = filetime::FileTime::from_unix_time(0, 0);
        let entry = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        filetime::set_file_mtime(entry.path(), old).unwrap();

        let fetched = fetch_image_tags(&cache, "registry.invalid/foo/bar", true)
            .await
            .expect("stale cache must satisfy an unreachable registry");
        assert_eq!(fetched.tags, cached);
        assert_eq!(fetched.source, TagSource::StaleCache);
    }

    /// The messy real-world case: 1924 tags, both spellings of every numeric
    /// variant, and lexical registry ordering.
    #[test]
    fn ranks_the_real_mcr_base_tag_list() {
        let raw = include_str!("../testdata/mcr-devcontainers-base-tags.json");
        let tags: Vec<String> = serde_json::from_str::<serde_json::Value>(raw).unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        assert!(tags.len() > 1000, "fixture should be the full list");

        // Both numeric spellings are live in this data and must both rank.
        assert!(!pinnable_tags(&refs, "alpine3.20").is_empty());
        assert!(!pinnable_tags(&refs, "ubuntu-24.04").is_empty());
        assert!(!pinnable_tags(&refs, "ubuntu24.04").is_empty());

        // The lexical trap: last-in-list is not newest.
        let newest = pinnable_tags(&refs, "bookworm").first().copied().unwrap();
        let last_lexical = refs
            .iter()
            .rfind(|t| t.ends_with("-bookworm"))
            .copied()
            .unwrap();
        assert_ne!(newest, last_lexical);

        // And the ranked head really is the maximum by version key.
        let newest_key = split_tag(newest)
            .and_then(|(version, _)| version_key(version))
            .unwrap();
        for tag in pinnable_tags(&refs, "bookworm") {
            let key = split_tag(tag)
                .and_then(|(version, _)| version_key(version))
                .unwrap();
            assert!(key <= newest_key, "{tag} outranks the reported newest");
        }
    }

    #[test]
    fn pinnable_tags_truncates_to_max() {
        let owned: Vec<String> = (0..MAX_PINNED_TAGS + 5)
            .map(|i| format!("4.0.{i}-trixie"))
            .collect();
        let tags: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pinned = pinnable_tags(&tags, "trixie");
        assert_eq!(pinned.len(), MAX_PINNED_TAGS);
        // Newest patch first after numeric (not lexicographic) sorting.
        assert_eq!(pinned[0], format!("4.0.{}-trixie", MAX_PINNED_TAGS + 4));
    }
}
