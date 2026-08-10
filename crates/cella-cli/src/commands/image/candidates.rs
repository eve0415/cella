//! Turns a pinned tag plus a published tag list into the updates worth offering.

use std::collections::BTreeSet;

use cella_oci::{pinnable_tags, split_tag, version_key};

use super::release::parse_variant;

/// A move to a newer release of the same distro family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsMove {
    /// The tag to pin, newest within the target variant.
    pub tag: String,
    /// The variant currently pinned, e.g. `bullseye`.
    pub from: String,
    /// The variant being moved to, e.g. `trixie`.
    pub to: String,
}

/// Everything worth offering for one pinned tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidates {
    /// The tag currently pinned.
    pub current: String,
    /// Newest tag in the current variant, if it is newer than `current`.
    pub version_bump: Option<String>,
    /// Newest tag in each newer release of the same family, newest first.
    pub os_moves: Vec<OsMove>,
}

impl Candidates {
    /// Whether there is nothing to offer.
    pub fn is_empty(&self) -> bool {
        self.version_bump.is_none() && self.os_moves.is_empty()
    }
}

/// Compute candidates for `current`, or `None` when `current` is a floating
/// tag with no version to advance (`latest`, `trixie`, `dev-1-trixie`).
pub fn compute(tags: &[String], current: &str) -> Option<Candidates> {
    let (current_version, variant) = split_tag(current)?;
    let refs: Vec<&str> = tags.iter().map(String::as_str).collect();

    let version_bump = newest_in_variant(&refs, variant)
        .filter(|best| *best != current)
        .filter(|best| {
            // `pinnable_tags` ranks within a variant but knows nothing about
            // what is currently pinned. A "newest" that is not actually newer
            // than the pin must not be offered as an upgrade.
            let Some(current_key) = version_key(current_version) else {
                return false;
            };
            split_tag(best)
                .and_then(|(version, _)| version_key(version))
                .is_some_and(|key| key > current_key)
        })
        .map(str::to_owned);

    let os_moves = parse_variant(variant).map_or_else(Vec::new, |current_release| {
        // Dedupe variants first: a repository publishes many tags per
        // variant, and only the newest of each is ever offered.
        let mut targets: Vec<(u32, &str)> = refs
            .iter()
            .filter_map(|t| split_tag(t).map(|(_, v)| v))
            .filter(|v| !v.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|v| {
                let release = parse_variant(v)?;
                release
                    .is_newer_than(&current_release)
                    .then_some((release.ord, v))
            })
            .collect();
        targets.sort_unstable_by_key(|(ord, _)| std::cmp::Reverse(*ord));

        targets
            .into_iter()
            .filter_map(|(_, target)| {
                Some(OsMove {
                    tag: newest_in_variant(&refs, target)?.to_owned(),
                    from: variant.to_owned(),
                    to: target.to_owned(),
                })
            })
            .collect()
    });

    Some(Candidates {
        current: current.to_owned(),
        version_bump,
        os_moves,
    })
}

/// Newest tag carrying `variant`.
///
/// An empty variant means the tag *is* the version (`ubuntu:24.04`), so rank
/// purely numeric tags directly — `pinnable_tags` cannot express an empty
/// selection.
fn newest_in_variant<'a>(tags: &[&'a str], variant: &str) -> Option<&'a str> {
    if variant.is_empty() {
        return tags
            .iter()
            .filter_map(|t| match split_tag(t) {
                Some((version, "")) => version_key(version).map(|key| (key, *t)),
                _ => None,
            })
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, tag)| tag);
    }
    pinnable_tags(tags, variant).first().copied()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Newest `-trixie` tag in the committed fixture, verified against it by
    /// reading the file — MCR publishes continuously, so this is pinned to
    /// the captured data rather than to whatever upstream holds today.
    const NEWEST_TRIXIE: &str = "2.0.14-1-trixie";

    fn rust_tags() -> Vec<String> {
        let raw = include_str!("../../../testdata/mcr-devcontainers-rust-tags.json");
        serde_json::from_str::<serde_json::Value>(raw).unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn version_bump_beats_lexical_order() {
        // The registry returns tags lexically, so the last `-trixie` entry in
        // the raw list is a `dev-` build and the real newest sits mid-list.
        // This is the bug being prevented.
        let tags = rust_tags();
        let c = compute(&tags, "2.0.2-trixie").unwrap();
        let bump = c.version_bump.expect("a newer trixie tag exists");
        assert!(bump.ends_with("-trixie"));

        let lexically_last = tags
            .iter()
            .filter(|t| t.ends_with("-trixie"))
            .next_back()
            .unwrap();
        assert_ne!(&bump, lexically_last, "lexically last is not newest");
    }

    #[test]
    fn offers_os_moves_only_forward_and_in_family() {
        let c = compute(&rust_tags(), "2.0.2-bullseye").unwrap();
        let targets: Vec<&str> = c.os_moves.iter().map(|m| m.to.as_str()).collect();
        assert!(targets.contains(&"trixie"));
        assert!(targets.contains(&"bookworm"));
        assert!(
            !targets.contains(&"buster"),
            "buster is older than bullseye"
        );
        assert!(!targets.contains(&"bullseye"), "same release is not a move");
    }

    #[test]
    fn os_moves_are_newest_release_first() {
        let c = compute(&rust_tags(), "2.0.2-buster").unwrap();
        let targets: Vec<&str> = c.os_moves.iter().map(|m| m.to.as_str()).collect();
        assert_eq!(targets, vec!["trixie", "bookworm", "bullseye"]);
        assert!(c.os_moves.iter().all(|m| m.from == "buster"));
    }

    #[test]
    fn floating_tags_produce_nothing() {
        assert!(compute(&rust_tags(), "latest").is_none());
        assert!(compute(&rust_tags(), "trixie").is_none());
    }

    #[test]
    fn already_newest_offers_no_bump() {
        // Pinned as a literal from the committed fixture. Deriving it by
        // calling compute() would let a comparator that is wrong in one
        // direction agree with itself and pass.
        let tags = rust_tags();
        assert!(
            tags.iter().any(|t| t == NEWEST_TRIXIE),
            "fixture must contain {NEWEST_TRIXIE}"
        );

        let c = compute(&tags, NEWEST_TRIXIE).unwrap();
        assert_eq!(c.version_bump, None);
        assert_eq!(
            compute(&tags, "2.0.2-trixie")
                .unwrap()
                .version_bump
                .as_deref(),
            Some(NEWEST_TRIXIE)
        );
    }

    #[test]
    fn trixie_is_newest_debian_so_it_has_no_os_moves() {
        let c = compute(&rust_tags(), NEWEST_TRIXIE).unwrap();
        assert!(c.os_moves.is_empty());
        assert!(c.is_empty());
    }

    #[test]
    fn no_variant_tags_get_version_bumps_and_no_os_moves() {
        // `ubuntu:24.04` — the whole tag is the version, so there is no
        // suffix to hold fixed and no OS move to offer.
        let tags: Vec<String> = ["24.04", "24.10", "22.04", "latest", "noble"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let c = compute(&tags, "24.04").unwrap();
        assert_eq!(c.version_bump.as_deref(), Some("24.10"));
        assert!(c.os_moves.is_empty());

        assert_eq!(compute(&tags, "24.10").unwrap().version_bump, None);
    }
}
