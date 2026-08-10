//! Structure and ordering of OCI image tags.
//!
//! Registries return `/v2/<name>/tags/list` in lexical order, which puts
//! `2.0.9-trixie` after `2.0.14-trixie`. Everything here exists so callers
//! rank tags by version instead of by string.

use tracing::warn;

use crate::tag_cache::TagCache;

/// Maximum number of pinned tags to present to the user.
pub const MAX_PINNED_TAGS: usize = 15;

/// Select the tags that pin a variant selection to a specific image
/// version, newest first.
///
/// A tag qualifies when it refines the selection in one of two ways:
///
/// - **Version prefix** — it ends with `-{selection}` and everything
///   before that suffix is dot/dash-separated numbers: for `"24-trixie"`
///   this matches `4.0.10-24-trixie` and `5-24-trixie`.
/// - **Version extension** — it extends the selection's own leading
///   version with more dotted segments: for `"24-trixie"` this matches
///   `24.7.0-trixie` (node-style registries).
///
/// Neither shape matches `dev-24-trixie`, the bare `24-trixie`, or
/// another variant's `22-trixie`.
///
/// Tags are sorted by their version part descending, with more specific
/// versions ranking above their aliases (`4.0.10` > `4.0` > `4`), and
/// capped at [`MAX_PINNED_TAGS`].
pub fn pinnable_tags<'a>(tags: &[&'a str], selection: &str) -> Vec<&'a str> {
    let mut keyed: Vec<(VersionKey, &'a str)> = tags
        .iter()
        .filter_map(|&tag| Some((pin_version_key(tag, selection)?, tag)))
        .collect();
    keyed.sort_unstable_by(|a, b| b.cmp(a));
    keyed.truncate(MAX_PINNED_TAGS);
    keyed.into_iter().map(|(_, tag)| tag).collect()
}

/// Compute the version sort key of a tag that refines `selection`, or
/// `None` if it doesn't (see [`pinnable_tags`] for the accepted shapes).
fn pin_version_key(tag: &str, selection: &str) -> Option<VersionKey> {
    // Version prefix: `{version}-{selection}`.
    if let Some(prefix) = tag
        .strip_suffix(selection)
        .and_then(|rest| rest.strip_suffix('-'))
        && let Some(key) = version_key(prefix)
    {
        return Some(key);
    }

    // Version extension: the selection's leading numeric part grows more
    // dotted segments (`24-trixie` → `24.7.0-trixie`, `22` → `22.12.0`).
    let numeric_end = selection
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(selection.len());
    let (head, rest) = selection.split_at(numeric_end);
    if head.is_empty() {
        return None;
    }
    let extended = tag.strip_suffix(rest)?;
    if !extended.strip_prefix(head)?.starts_with('.') {
        return None;
    }
    version_key(extended)
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
        let numeric = !group.is_empty()
            && group
                .split('.')
                .all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()));
        if !numeric {
            break;
        }
        end += group.len() + 1;
    }
    if end == 0 {
        return None;
    }
    Some((&tag[..end - 1], tag.get(end..).unwrap_or("")))
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
        // index 1. Dash groups must be compared before dot segments.
        let tags = vec!["4-24-trixie", "4.0-24-trixie", "4.0.10-24-trixie"];
        assert_eq!(
            pinnable_tags(&tags, "trixie"),
            vec!["4.0.10-24-trixie", "4.0-24-trixie", "4-24-trixie"]
        );
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
