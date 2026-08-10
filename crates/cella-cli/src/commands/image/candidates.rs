//! Turns a pinned tag plus a published tag list into the updates worth offering.

use std::collections::BTreeMap;

use cella_oci::{VersionKey, split_tag, version_key};

use super::release::{Release, is_codename, parse_variant};

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
    /// An empty candidate set for a tag cella cannot rank.
    ///
    /// Used when the current tag is floating (`latest`, `trixie`) but the
    /// user named a target explicitly — there is nothing to offer, yet there
    /// is still something to apply.
    pub fn floating(current: &str) -> Self {
        Self {
            current: current.to_owned(),
            version_bump: None,
            os_moves: Vec::new(),
        }
    }

    /// Whether there is nothing to offer.
    pub const fn is_empty(&self) -> bool {
        self.version_bump.is_none() && self.os_moves.is_empty()
    }
}

/// Compute candidates for `current`, or `None` when `current` is a floating
/// tag with no version to advance (`latest`, `trixie`, `dev-1-trixie`).
pub fn compute(tags: &[String], current: &str) -> Option<Candidates> {
    let (current_version, selection) = split_pin(current)?;
    let refs: Vec<&str> = tags.iter().map(String::as_str).collect();

    // Ranking finds the newest tag sharing the pin's selection; it says
    // nothing about whether that tag beats the pin, so every candidate is
    // measured against this key before being offered. An unparseable pin
    // leaves nothing to measure against, so nothing is offered.
    let Some(current_key) = version_key(current_version) else {
        return Some(Candidates::floating(current));
    };

    let version_bump = newest_in_variant(&refs, selection)
        .filter(|(key, tag)| *tag != current && *key > current_key)
        .map(|(_, tag)| tag.to_owned());

    let os_moves = distro_of(selection).map_or_else(Vec::new, |(prefix, _, current_release)| {
        newer_releases(&refs, &current_release)
            .into_iter()
            .filter_map(|(_, target_distro)| {
                // Rebuild the full selection so an OS move carries the pinned
                // prefix across: `22-bookworm` moves to `22-trixie`, never to
                // whatever the newest `-trixie` tag happens to be.
                let target = format!("{prefix}{target_distro}");
                let (key, tag) = newest_in_variant(&refs, &target)?;
                // A newer OS whose version stream has only just started would
                // roll the image version backwards. Moving forward on one axis
                // is not worth moving backwards on the other.
                (key >= current_key).then(|| OsMove {
                    tag: tag.to_owned(),
                    from: selection.to_owned(),
                    to: target,
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

/// Split a pinned tag into the image's own version and the selection that
/// must be held fixed.
///
/// [`cella_oci::split_tag`] puts *every* leading numeric group in the version,
/// but only the first is the image's version. A composite tag like
/// `4.0.10-22-trixie` (typescript-node) encodes a runtime major the user
/// pinned deliberately, so everything after the first group belongs to the
/// selection: offering `5.0.1-24-trixie` would move Node 22 to Node 24.
fn split_pin(tag: &str) -> Option<(&str, &str)> {
    let (version, variant) = split_tag(tag)?;
    if variant.is_empty() {
        return Some((version, ""));
    }
    version.find('-').map_or(Some((version, variant)), |cut| {
        Some((&tag[..cut], &tag[cut + 1..]))
    })
}

/// Locate the distro-release portion of a selection.
///
/// Returns the prefix that must be preserved across an OS move, the distro
/// spelling itself, and the release it denotes. For `22-trixie` this is
/// `("22-", "trixie", Debian 13)`; for `ubuntu-24.04` it is
/// `("", "ubuntu-24.04", Ubuntu 24.04)`.
fn distro_of(selection: &str) -> Option<(&str, &str, Release)> {
    let mut start = 0;
    loop {
        let rest = &selection[start..];
        if let Some(release) = parse_variant(rest) {
            return Some((&selection[..start], rest, release));
        }
        start += rest.find('-')? + 1;
    }
}

/// Every release newer than `current` that the tag list publishes, newest
/// first, as `(ordinal, canonical spelling)`.
///
/// A repository may publish several aliases of one release — `devcontainers/base`
/// ships `bookworm`, `debian-12` and `debian12` — so releases are deduplicated
/// by identity rather than by spelling, and the codename wins as the canonical
/// form. Without this the user sees the same move three times and a `--yes
/// --allow-os-change` tie picks an arbitrary alias.
fn newer_releases<'a>(tags: &[&'a str], current: &Release) -> Vec<(u32, &'a str)> {
    let mut best: BTreeMap<(&str, u32), &'a str> = BTreeMap::new();

    for &tag in tags {
        let Some((_, selection)) = split_pin(tag) else {
            continue;
        };
        let Some((_, distro, release)) = distro_of(selection) else {
            continue;
        };
        if !release.is_newer_than(current) {
            continue;
        }
        best.entry((release.family, release.ord))
            .and_modify(|chosen| {
                if prefers(distro, chosen) {
                    *chosen = distro;
                }
            })
            .or_insert(distro);
    }

    let mut targets: Vec<(u32, &'a str)> = best
        .into_iter()
        .map(|((_, ord), distro)| (ord, distro))
        .collect();
    targets.sort_unstable_by_key(|(ord, distro)| (std::cmp::Reverse(*ord), *distro));
    targets
}

/// Whether `candidate` is the better spelling of a release than `current`.
///
/// Codename first (that is what containers.dev documents), then shortest,
/// then lexical — so the choice is total and does not depend on tag order.
fn prefers(candidate: &str, current: &str) -> bool {
    (is_codename(candidate), current.len(), current)
        > (is_codename(current), candidate.len(), candidate)
}

/// Newest tag whose selection is exactly `selection`.
///
/// Split the same way as the pin, so ranking and the newer-than guard agree
/// on what counts as the version. Matching a looser suffix instead would rank
/// `2.0.14-1-trixie` as the newest `-trixie` tag even for a pin on the plain
/// `-trixie` line, then reject it for having the same version — reporting
/// "up to date" while a genuinely newer plain tag sat unoffered.
///
/// An empty selection means the tag *is* the version (`ubuntu:24.04`); it
/// needs no special case here.
///
/// The winning [`VersionKey`] is returned alongside the tag so callers can
/// compare it against the pin without parsing the tag a second time.
fn newest_in_variant<'a>(tags: &[&'a str], selection: &str) -> Option<(VersionKey, &'a str)> {
    tags.iter()
        .filter_map(|t| {
            let (version, tag_selection) = split_pin(t)?;
            if tag_selection != selection {
                return None;
            }
            version_key(version).map(|key| (key, *t))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
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

    /// Newest tag on the fixture's *plain* `-trixie` line, i.e. with no
    /// revision group.
    const NEWEST_PLAIN_TRIXIE: &str = "2.0.14-trixie";

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

        let lexically_last = tags.iter().rfind(|t| t.ends_with("-trixie")).unwrap();
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

    /// Regression: `split_tag` puts every leading numeric group in the
    /// version, so `4.0.10-22-trixie` looked like version `4.0.10-22` of
    /// variant `trixie` and ranked against *every* `-trixie` tag. A user
    /// pinned to Node 22 was offered Node 24 — and `--yes` would take it.
    #[test]
    fn a_pinned_runtime_major_is_never_silently_changed() {
        let tags: Vec<String> = [
            "5.0.1-24-trixie",
            "4.0.10-24-trixie",
            "5.0.1-22-trixie",
            "4.0.10-22-trixie",
            "22-trixie",
            "24-trixie",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        let c = compute(&tags, "4.0.10-22-trixie").unwrap();
        assert_eq!(c.version_bump.as_deref(), Some("5.0.1-22-trixie"));
    }

    #[test]
    fn an_os_move_preserves_the_pinned_runtime_major() {
        let tags: Vec<String> = [
            "4.0.10-22-bookworm",
            "5.0.1-22-trixie",
            "5.0.1-24-trixie",
            "4.0.10-24-bookworm",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        let c = compute(&tags, "4.0.10-22-bookworm").unwrap();
        assert_eq!(c.os_moves.len(), 1, "got {:?}", c.os_moves);
        assert_eq!(c.os_moves[0].to, "22-trixie");
        assert_eq!(
            c.os_moves[0].tag, "5.0.1-22-trixie",
            "moving OS must not also move Node"
        );
    }

    /// Regression: `devcontainers/base` publishes `bookworm`, `debian-12` and
    /// `debian12` for one release. Deduping the strings before mapping them
    /// to releases produced three identical choices, and the tie made
    /// `--yes --allow-os-change` pick an arbitrary spelling.
    #[test]
    fn aliases_of_one_release_collapse_to_a_single_move() {
        let tags: Vec<String> = [
            "1.0.0-bullseye",
            "1.0.0-bookworm",
            "1.0.0-debian-12",
            "1.0.0-debian12",
            "1.0.1-bookworm",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        let c = compute(&tags, "1.0.0-bullseye").unwrap();
        assert_eq!(c.os_moves.len(), 1, "got {:?}", c.os_moves);
        assert_eq!(
            c.os_moves[0].to, "bookworm",
            "the codename is the canonical spelling"
        );
        assert_eq!(c.os_moves[0].tag, "1.0.1-bookworm");
    }

    /// The real messy repository: every numeric variant has two spellings, so
    /// a target list with duplicates would show the same release twice.
    #[test]
    fn the_real_base_image_offers_one_move_per_release() {
        let raw = include_str!("../../../../cella-oci/testdata/mcr-devcontainers-base-tags.json");
        let tags: Vec<String> = serde_json::from_str::<serde_json::Value>(raw).unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();

        let c = compute(&tags, "1.0.0-bullseye").unwrap();
        let mut ordinals: Vec<&str> = c.os_moves.iter().map(|m| m.to.as_str()).collect();
        let before = ordinals.len();
        ordinals.sort_unstable();
        ordinals.dedup();
        assert_eq!(ordinals.len(), before, "duplicate targets: {ordinals:?}");
        assert!(
            c.os_moves.iter().all(|m| parse_variant(&m.to).is_some()),
            "every target must be a recognised release: {:?}",
            c.os_moves
        );
    }

    /// Regression: `version_bump` was guarded against a "newest" that is not
    /// actually newer, but `os_moves` was not. A release whose version stream
    /// has only just started would roll the image version backwards, and
    /// `--yes --allow-os-change` would take it without asking.
    #[test]
    fn an_os_move_is_never_a_version_downgrade() {
        let tags: Vec<String> = ["3.0.4-bookworm", "1.0.0-trixie", "3.0.4-trixie"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        // trixie's newest is 3.0.4 here, which is not a downgrade.
        let ok = compute(&tags, "3.0.4-bookworm").unwrap();
        assert_eq!(ok.os_moves.len(), 1);
        assert_eq!(ok.os_moves[0].tag, "3.0.4-trixie");

        // Drop it, and the only trixie tag left is two majors behind.
        let young: Vec<String> = ["3.0.4-bookworm", "1.0.0-trixie"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let c = compute(&young, "3.0.4-bookworm").unwrap();
        assert!(
            c.os_moves.is_empty(),
            "a move that rolls the version back is not an update: {:?}",
            c.os_moves
        );
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

        // MCR publishes two parallel lines: plain `X-trixie` and revision
        // `X-1-trixie`. A pin stays on the line it is on, so the plain line's
        // newest is what a plain pin is offered.
        assert_eq!(
            compute(&tags, "2.0.2-trixie")
                .unwrap()
                .version_bump
                .as_deref(),
            Some(NEWEST_PLAIN_TRIXIE)
        );
        assert_eq!(
            compute(&tags, NEWEST_PLAIN_TRIXIE).unwrap().version_bump,
            None
        );
    }

    /// The two tag lines must not be crossed in either direction: a revision
    /// pin is not an upgrade for a plain pin, nor the reverse.
    #[test]
    fn a_pin_stays_on_its_own_tag_line() {
        let tags = rust_tags();
        assert!(tags.iter().any(|t| t == NEWEST_PLAIN_TRIXIE));

        for (pin, expected) in [
            ("2.0.9-trixie", Some(NEWEST_PLAIN_TRIXIE)),
            ("2.0.9-1-trixie", Some(NEWEST_TRIXIE)),
        ] {
            assert_eq!(
                compute(&tags, pin).unwrap().version_bump.as_deref(),
                expected,
                "pin {pin} crossed lines"
            );
        }
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
