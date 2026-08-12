//! Turns a pinned tag plus a published tag list into the updates worth offering.

use std::collections::{BTreeMap, BTreeSet};

use cella_oci::{TagGrammar, VersionKey, version_key};

use super::release::{Release, is_codename, parse_variant};

/// Which properties of the pinned tag a move changes.
///
/// A move always changes at least one, so an empty set marks the pin's own
/// variant rather than a candidate. Keeping them separate is what lets
/// `--yes` accept a version-preserving move while refusing a runtime change:
/// without the distinction a shape-only move would carry no axis at all and
/// pass every gate vacuously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AxisSet(u8);

impl AxisSet {
    /// Making an implicit pin explicit: `5.0.3-trixie` → `5.0.3-24-trixie`.
    /// The two resolve to the same image today, but the alias will move on
    /// and the explicit tag will not.
    pub const SHAPE: Self = Self(1);
    /// A different runtime major: Node 22 → Node 24.
    pub const RUNTIME: Self = Self(2);
    /// A newer distro release: bookworm → trixie.
    pub const RELEASE: Self = Self(4);

    /// Whether this move changes nothing at all.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every axis in `other` is present here.
    ///
    /// This is the `--yes` gate: a move may be taken only when each axis it
    /// changes has been allowed.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The axis names, in a stable order, for display and JSON.
    pub fn names(self) -> Vec<&'static str> {
        [
            (Self::SHAPE, "shape"),
            (Self::RUNTIME, "runtime"),
            (Self::RELEASE, "release"),
        ]
        .into_iter()
        .filter(|(axis, _)| self.contains(*axis))
        .map(|(_, name)| name)
        .collect()
    }
}

impl std::ops::BitOr for AxisSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for AxisSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A move from the pinned variant to a different published one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantMove {
    /// The tag to pin, newest within the target variant.
    pub tag: String,
    /// The variant currently pinned, e.g. `22-bookworm`.
    pub from: String,
    /// The variant being moved to, e.g. `24-trixie`.
    pub to: String,
    /// What this move changes. Never empty.
    pub axes: AxisSet,
}

/// Why part of the offer is missing.
///
/// Surfaced rather than silently shortening the list: a user who sees three
/// moves has no way to tell "these are all of them" from "runtimes could not
/// be read".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limitation {
    /// The variant does not decompose into runtime and release — php's
    /// `8.5-apache-trixie` leaves `8.5-apache`, which is not a version.
    UndecomposableVariant,
}

impl Limitation {
    /// The one-line explanation shown under the candidate list.
    pub const fn message(self) -> &'static str {
        match self {
            Self::UndecomposableVariant => "runtime moves unavailable for this variant shape",
        }
    }
}

/// The runtime component of a variant, as far as it can be ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Runtime<'a> {
    /// No runtime component — an alias line such as `trixie`, which resolves
    /// to whichever runtime upstream currently points it at.
    Alias,
    /// A comparable numeric runtime: `22-trixie` yields `22`.
    Explicit(&'a str, VersionKey),
    /// A prefix that is not a version, so runtimes here cannot be ordered.
    Opaque(&'a str),
}

/// Read the runtime component out of the prefix that [`distro_of`] returns.
fn runtime_of(prefix: &str) -> Runtime<'_> {
    let Some(runtime) = prefix.strip_suffix('-') else {
        return Runtime::Alias;
    };
    version_key(runtime).map_or(Runtime::Opaque(runtime), |key| {
        Runtime::Explicit(runtime, key)
    })
}

/// Which axes separate a candidate variant from the pinned one, or `None`
/// when the candidate is not a forward move at all.
///
/// Forward-only on both axes: the published vocabularies still carry EOL
/// entries (`14-buster`, `3.6-bullseye`), and those must never be offered as
/// updates.
fn axes_between(
    pin: &Runtime<'_>,
    candidate: &Runtime<'_>,
    release_changed: bool,
) -> Option<AxisSet> {
    let mut axes = if release_changed {
        AxisSet::RELEASE
    } else {
        AxisSet::default()
    };

    match (pin, candidate) {
        (Runtime::Alias, Runtime::Alias) => {}
        (Runtime::Explicit(_, pinned), Runtime::Explicit(_, target)) => {
            if target < pinned {
                return None;
            }
            if target > pinned {
                axes |= AxisSet::RUNTIME;
            }
        }
        // Two opaque prefixes are comparable only when identical, which
        // leaves the release as the single axis that can move.
        (Runtime::Opaque(pinned), Runtime::Opaque(target)) if pinned == target => {}
        // Everything else is incomparable: an alias pin's runtime is whatever
        // upstream currently points it at, and two unlike opaque prefixes say
        // nothing about each other. Offering these unanchored would present
        // Node 22 as an update to a Node 24 pin.
        _ => return None,
    }

    (!axes.is_empty()).then_some(axes)
}

/// Everything worth offering for one pinned tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidates {
    /// The tag currently pinned.
    pub current: String,
    /// Newest tag in the current variant, when the pin already carries a
    /// version and that tag is newer.
    pub version_bump: Option<String>,
    /// Newest tag in the current variant, when the pin is floating.
    ///
    /// Mutually exclusive with [`Self::version_bump`] by construction: a pin
    /// either has a version to advance or has none to begin with.
    pub pin: Option<String>,
    /// Every forward move off the pinned variant, newest release first.
    pub moves: Vec<VariantMove>,
    /// Why part of the offer is missing, when something is.
    pub limitation: Option<Limitation>,
}

impl Candidates {
    /// An empty candidate set for a tag cella cannot rank at all.
    ///
    /// Used when the current tag does not parse (`latest`, `dev-1-trixie`)
    /// but the user named a target explicitly — there is nothing to offer,
    /// yet there is still something to apply.
    pub fn unrankable(current: &str) -> Self {
        Self {
            current: current.to_owned(),
            version_bump: None,
            pin: None,
            moves: Vec::new(),
            limitation: None,
        }
    }

    /// Whether there is nothing to offer.
    pub const fn is_empty(&self) -> bool {
        self.version_bump.is_none() && self.pin.is_none() && self.moves.is_empty()
    }
}

/// Compute candidates for `current`, or `None` when `current` has no version
/// to advance — either it does not parse (`latest`, `dev-1-trixie`) or it is a
/// floating tag naming a variant only (`trixie`, `24-trixie`).
///
/// The pin is read through the repository's own tag grammar rather than by
/// tag shape. Shape cannot tell `5.0.3-trixie` (image version 5.0.3) from
/// `24-trixie` (the floating Node 24 variant): both are one leading numeric
/// group. Reading them the same way ranked `24 > 5` and offered a floating
/// tag as an update to a pinned one.
pub fn compute(tags: &[String], current: &str) -> Option<Candidates> {
    let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    // Derived once for the whole computation: every parse below shares it.
    let grammar = TagGrammar::from_tags(&refs);

    let parsed = grammar.parse(current)?;
    let selection = parsed.variant;

    // A floating pin names a variant but carries no image version, so there
    // is nothing to advance from — only something to fix in place. Offering
    // that is the whole point of a command that pins.
    let Some(current_version) = parsed.version else {
        return Some(Candidates {
            current: current.to_owned(),
            version_bump: None,
            pin: newest_in_variant(&grammar, &refs, selection).map(|(_, tag)| tag.to_owned()),
            moves: moves_for(&grammar, &refs, selection, None),
            limitation: limitation_for(selection),
        });
    };

    // Ranking finds the newest tag sharing the pin's selection; it says
    // nothing about whether that tag beats the pin, so every candidate is
    // measured against this key before being offered. An unparseable pin
    // leaves nothing to measure against, so nothing is offered.
    let Some(current_key) = version_key(current_version) else {
        return Some(Candidates::unrankable(current));
    };

    let version_bump = newest_in_variant(&grammar, &refs, selection)
        .filter(|(key, tag)| *tag != current && *key > current_key)
        .map(|(_, tag)| tag.to_owned());

    Some(Candidates {
        current: current.to_owned(),
        version_bump,
        pin: None,
        moves: moves_for(&grammar, &refs, selection, Some(&current_key)),
        limitation: limitation_for(selection),
    })
}

/// The limitation that applies to a pin on `selection`, if any.
fn limitation_for(selection: &str) -> Option<Limitation> {
    let (prefix, _, _) = distro_of(selection)?;
    matches!(runtime_of(prefix), Runtime::Opaque(_)).then_some(Limitation::UndecomposableVariant)
}

/// Every published variant, as the grammar reads them, with alias spellings
/// of one release collapsed to a single entry.
///
/// A repository may publish several spellings of one release —
/// `devcontainers/base` ships `bookworm`, `debian-12` and `debian12` — so
/// variants are keyed by the release they denote and the prefix they carry,
/// not by their text. Without this the user sees the same move three times
/// and a `--yes` tie picks an arbitrary spelling.
fn published_variants<'a>(grammar: &TagGrammar, tags: &[&'a str]) -> Vec<&'a str> {
    let mut best: BTreeMap<(&str, u32, &str), (&'a str, &'a str)> = BTreeMap::new();
    let mut opaque: BTreeSet<&'a str> = BTreeSet::new();

    for variant in tags
        .iter()
        .filter_map(|tag| grammar.parse(tag).map(|parsed| parsed.variant))
    {
        // A variant cella cannot map to a release has no alias to collapse
        // against, so it is kept verbatim.
        let Some((prefix, distro, release)) = distro_of(variant) else {
            opaque.insert(variant);
            continue;
        };
        best.entry((release.family, release.ord, prefix))
            .and_modify(|chosen| {
                if prefers(distro, chosen.1) {
                    *chosen = (variant, distro);
                }
            })
            .or_insert((variant, distro));
    }

    best.into_values()
        .map(|(variant, _)| variant)
        .chain(opaque)
        .collect()
}

/// Whether `candidate` is the better spelling of a release than `current`.
///
/// Codename first (that is what containers.dev documents), then shortest,
/// then lexical — so the choice is total and does not depend on tag order.
fn prefers(candidate: &str, current: &str) -> bool {
    (is_codename(candidate), current.len(), current)
        > (is_codename(current), candidate.len(), candidate)
}

/// The full forward cross product of moves off `selection`.
///
/// Every published variant whose runtime is at least the pin's and whose
/// release is at least the pin's, excluding the pin's own variant, each
/// carrying the newest published version in that variant.
///
/// `current_key` is the pin's version, when it has one. A move that would
/// roll the image version backwards is not an update, so it is dropped —
/// but a floating pin has no version to protect and so applies no such
/// filter.
fn moves_for(
    grammar: &TagGrammar,
    tags: &[&str],
    selection: &str,
    current_key: Option<&VersionKey>,
) -> Vec<VariantMove> {
    let Some((prefix, _, current_release)) = distro_of(selection) else {
        return Vec::new();
    };
    let pin_runtime = runtime_of(prefix);

    let mut moves: Vec<(u32, Option<VersionKey>, VersionKey, VariantMove)> = Vec::new();
    for candidate in published_variants(grammar, tags) {
        if candidate == selection {
            continue;
        }
        let Some((candidate_prefix, _, release)) = distro_of(candidate) else {
            continue;
        };
        // Same release or newer, never a different family.
        if release != current_release && !release.is_newer_than(&current_release) {
            continue;
        }
        let Some(axes) = axes_between(
            &pin_runtime,
            &runtime_of(candidate_prefix),
            release.ord != current_release.ord,
        ) else {
            continue;
        };
        let Some((key, tag)) = newest_in_variant(grammar, tags, candidate) else {
            continue;
        };
        if current_key.is_some_and(|current| &key < current) {
            continue;
        }
        let runtime_rank = match runtime_of(candidate_prefix) {
            Runtime::Explicit(_, key) => Some(key),
            Runtime::Alias | Runtime::Opaque(_) => None,
        };
        moves.push((
            release.ord,
            runtime_rank,
            key,
            VariantMove {
                tag: tag.to_owned(),
                from: selection.to_owned(),
                to: candidate.to_owned(),
                axes,
            },
        ));
    }

    // Newest release first, then newest runtime, then newest version, then by
    // name so the order never depends on how the registry listed its tags.
    moves.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.3.to.cmp(&b.3.to))
    });
    moves.into_iter().map(|(_, _, _, m)| m).collect()
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

/// Newest tag whose variant is exactly `selection`.
///
/// Parsed through the same grammar as the pin, so ranking and the newer-than
/// guard agree on what counts as the version. Matching a looser suffix instead
/// would rank `2.0.14-1-trixie` as the newest `-trixie` tag even for a pin on
/// the plain `-trixie` line, then reject it for having the same version —
/// reporting "up to date" while a genuinely newer plain tag sat unoffered.
///
/// Floating tags carry no version and so never win: `24-trixie` is not an
/// update to anything.
///
/// An empty selection means the tag *is* the version (`ubuntu:24.04`); it
/// needs no special case here.
///
/// The winning [`VersionKey`] is returned alongside the tag so callers can
/// compare it against the pin without parsing the tag a second time.
fn newest_in_variant<'a>(
    grammar: &TagGrammar,
    tags: &[&'a str],
    selection: &str,
) -> Option<(VersionKey, &'a str)> {
    tags.iter()
        .filter_map(|t| {
            let parsed = grammar.parse(t)?;
            if parsed.variant != selection {
                return None;
            }
            version_key(parsed.version?).map(|key| (key, *t))
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

    fn typescript_node_tags() -> Vec<String> {
        let raw = include_str!("../../../testdata/mcr-devcontainers-typescript-node-tags.json");
        serde_json::from_str::<serde_json::Value>(raw).unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect()
    }

    /// The reported bug: `cella image update` moved a pin from
    /// `5.0.3-trixie` to `24-trixie` — off the newest image release, onto a
    /// Node major masquerading as a version, and onto a floating tag at that.
    ///
    /// Both halves are asserted. Checking only that no bump is offered would
    /// also pass if the grammar rejected the pin outright, which would report
    /// "up to date" for a reason that is not true.
    #[test]
    fn a_node_major_is_never_offered_as_an_image_version_bump() {
        let tags = typescript_node_tags();

        let newest = compute(&tags, "5.0.3-trixie").expect("the pin must parse");
        assert_eq!(
            newest.version_bump, None,
            "5.0.3 is the newest release on the trixie line"
        );

        let older = compute(&tags, "5.0.1-trixie").expect("the pin must parse");
        assert_eq!(
            older.version_bump.as_deref(),
            Some("5.0.3-trixie"),
            "the bump must stay on the pin's own variant"
        );
    }

    /// A floating pin has no image version, so it has nothing to bump — and
    /// must certainly not be bumped to a *different* runtime's floating tag.
    /// What it gets instead is a pin onto its own line.
    #[test]
    fn a_floating_runtime_tag_is_not_bumped_to_another_runtime() {
        let tags = typescript_node_tags();
        // Node 20 never received a 5.x image release, so its line tops out at
        // 4.0.10. A pin stays on its own line even when that line is behind —
        // offering 5.0.3-24-trixie here would change the runtime, not update.
        for (pin, expected) in [
            ("22-trixie", "5.0.3-22-trixie"),
            ("20-trixie", "4.0.10-20-trixie"),
            ("24-trixie", "5.0.3-24-trixie"),
        ] {
            let c = compute(&tags, pin).expect("a floating tag still names a variant");
            assert_eq!(c.version_bump, None, "{pin} has no version to advance");
            assert_eq!(
                c.pin.as_deref(),
                Some(expected),
                "{pin} must be pinned on its own runtime line"
            );
        }
    }

    /// A bare codename is an alias line: it too gets pinned, and to the
    /// newest version on its *own* variant rather than a runtime-ful one.
    #[test]
    fn a_bare_codename_pins_to_its_own_line() {
        let c = compute(&typescript_node_tags(), "trixie").expect("trixie names a variant");
        assert_eq!(c.pin.as_deref(), Some("5.0.3-trixie"));
        assert_eq!(c.version_bump, None);
    }

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
    fn offers_moves_only_forward_and_in_family() {
        let c = compute(&rust_tags(), "2.0.2-bullseye").unwrap();
        let targets: Vec<&str> = c.moves.iter().map(|m| m.to.as_str()).collect();
        assert!(targets.contains(&"trixie"));
        assert!(targets.contains(&"bookworm"));
        assert!(
            !targets.contains(&"buster"),
            "buster is older than bullseye"
        );
        assert!(!targets.contains(&"bullseye"), "same release is not a move");
    }

    #[test]
    fn moves_are_newest_release_first() {
        let c = compute(&rust_tags(), "2.0.2-buster").unwrap();
        let targets: Vec<&str> = c.moves.iter().map(|m| m.to.as_str()).collect();
        assert_eq!(targets, vec!["trixie", "bookworm", "bullseye"]);
        assert!(c.moves.iter().all(|m| m.from == "buster"));
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
    fn a_move_preserves_the_pinned_runtime_major() {
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

        // The release-only move is the one that must hold Node fixed. Moving
        // the runtime too is a separate offer, and it says so on its face.
        let release_only: Vec<&VariantMove> = c
            .moves
            .iter()
            .filter(|m| m.axes == AxisSet::RELEASE)
            .collect();
        assert_eq!(release_only.len(), 1, "got {:?}", c.moves);
        assert_eq!(release_only[0].to, "22-trixie");
        assert_eq!(
            release_only[0].tag, "5.0.1-22-trixie",
            "moving OS must not also move Node"
        );

        // Every move that does change the runtime is labelled as such, so
        // `--yes` can refuse it.
        for m in c.moves.iter().filter(|m| m.to.starts_with("24-")) {
            assert!(
                m.axes.contains(AxisSet::RUNTIME),
                "unlabelled runtime change: {m:?}"
            );
        }
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
        assert_eq!(c.moves.len(), 1, "got {:?}", c.moves);
        assert_eq!(
            c.moves[0].to, "bookworm",
            "the codename is the canonical spelling"
        );
        assert_eq!(c.moves[0].tag, "1.0.1-bookworm");
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
        let mut ordinals: Vec<&str> = c.moves.iter().map(|m| m.to.as_str()).collect();
        let before = ordinals.len();
        ordinals.sort_unstable();
        ordinals.dedup();
        assert_eq!(ordinals.len(), before, "duplicate targets: {ordinals:?}");
        assert!(
            c.moves.iter().all(|m| parse_variant(&m.to).is_some()),
            "every target must be a recognised release: {:?}",
            c.moves
        );
    }

    /// Regression: `version_bump` was guarded against a "newest" that is not
    /// actually newer, but `os_moves` was not. A release whose version stream
    /// has only just started would roll the image version backwards, and
    /// `--yes --allow-os-change` would take it without asking.
    #[test]
    fn a_move_is_never_a_version_downgrade() {
        let tags: Vec<String> = ["3.0.4-bookworm", "1.0.0-trixie", "3.0.4-trixie"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        // trixie's newest is 3.0.4 here, which is not a downgrade.
        let ok = compute(&tags, "3.0.4-bookworm").unwrap();
        assert_eq!(ok.moves.len(), 1);
        assert_eq!(ok.moves[0].tag, "3.0.4-trixie");

        // Drop it, and the only trixie tag left is two majors behind.
        let young: Vec<String> = ["3.0.4-bookworm", "1.0.0-trixie"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let c = compute(&young, "3.0.4-bookworm").unwrap();
        assert!(
            c.moves.is_empty(),
            "a move that rolls the version back is not an update: {:?}",
            c.moves
        );
    }

    /// The cross product is every forward variant, not just newer releases.
    /// Sizes are pinned to the captured fixture; they were cross-checked
    /// against the live registry when it was captured.
    #[test]
    fn moves_are_the_full_forward_cross_product() {
        let c = compute(&typescript_node_tags(), "5.0.3-22-bookworm").unwrap();
        let listed: Vec<(&str, Vec<&str>)> = c
            .moves
            .iter()
            .map(|m| (m.to.as_str(), m.axes.names()))
            .collect();

        // Newest release first, and within a release the newest runtime.
        assert_eq!(
            listed,
            vec![
                ("24-trixie", vec!["runtime", "release"]),
                ("22-trixie", vec!["release"]),
                ("24-bookworm", vec!["runtime"]),
            ]
        );
        assert!(
            c.moves.iter().all(|m| !m.axes.is_empty()),
            "a move with no axis would pass every --yes gate vacuously"
        );
    }

    /// The vocabularies still carry EOL entries. Moving *to* one is never an
    /// update, on either axis.
    #[test]
    fn moves_never_go_backwards_on_either_axis() {
        let c = compute(&typescript_node_tags(), "5.0.3-22-bookworm").unwrap();
        for m in &c.moves {
            assert!(
                !m.to.ends_with("-buster") && !m.to.ends_with("-bullseye"),
                "older release offered: {m:?}"
            );
            for older in ["14-", "16-", "18-", "20-"] {
                assert!(!m.to.starts_with(older), "older runtime offered: {m:?}");
            }
        }
    }

    /// php publishes `8.5-apache-trixie`: the part before the distro is not a
    /// version, so runtimes cannot be ordered. Releases still can be, and the
    /// gap is reported rather than passed off as a complete list.
    #[test]
    fn an_undecomposable_variant_keeps_release_moves_and_says_what_is_missing() {
        let tags: Vec<String> = [
            "3.0.4-8.5-apache-bookworm",
            "3.0.4-8.5-apache-trixie",
            "3.0.5-8.5-apache-trixie",
            "3.0.4-8.4-apache-trixie",
            "8.5-apache-bookworm",
            "8.5-apache-trixie",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        let c = compute(&tags, "3.0.4-8.5-apache-bookworm").unwrap();
        assert_eq!(
            c.limitation,
            Some(Limitation::UndecomposableVariant),
            "the missing runtime axis must be stated"
        );

        let targets: Vec<&str> = c.moves.iter().map(|m| m.to.as_str()).collect();
        assert_eq!(targets, vec!["8.5-apache-trixie"]);
        assert_eq!(c.moves[0].axes, AxisSet::RELEASE);
        assert_eq!(c.moves[0].tag, "3.0.5-8.5-apache-trixie");
        assert!(
            !targets.contains(&"8.4-apache-trixie"),
            "8.4 is not comparable to 8.5 here, so it must not be offered"
        );
    }

    /// dotnet publishes `9.0-bookworm-slim` and `11.0-preview-resolute`,
    /// neither of which resolves to a release cella knows. That yields no
    /// moves — the point is that it degrades rather than panicking on the
    /// empty-prefix path.
    #[test]
    fn a_variant_with_no_recognised_release_yields_no_moves() {
        let tags: Vec<String> = [
            "2.1.4-9.0-bookworm-slim",
            "2.1.4-11.0-preview-resolute",
            "9.0-bookworm-slim",
            "11.0-preview-resolute",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        for pin in ["2.1.4-9.0-bookworm-slim", "2.1.4-11.0-preview-resolute"] {
            let c = compute(&tags, pin).unwrap();
            assert!(c.moves.is_empty(), "{pin} got {:?}", c.moves);
            assert_eq!(c.limitation, None);
        }
    }

    #[test]
    fn unparseable_tags_produce_nothing() {
        // `latest` names no variant at all, so there is nothing to pin it to.
        assert!(compute(&rust_tags(), "latest").is_none());

        // A bare codename does name a variant, so it is pinnable — the one
        // thing it is not is bumpable.
        let trixie = compute(&rust_tags(), "trixie").expect("trixie is a published variant");
        assert_eq!(trixie.version_bump, None);
        assert_eq!(trixie.pin.as_deref(), Some(NEWEST_PLAIN_TRIXIE));
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
    fn trixie_is_newest_debian_so_it_has_no_moves() {
        let c = compute(&rust_tags(), NEWEST_TRIXIE).unwrap();
        assert!(c.moves.is_empty());
        assert!(c.is_empty());
    }

    #[test]
    fn no_variant_tags_get_version_bumps_and_no_moves() {
        // `ubuntu:24.04` — the whole tag is the version, so there is no
        // suffix to hold fixed and no OS move to offer.
        let tags: Vec<String> = ["24.04", "24.10", "22.04", "latest", "noble"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let c = compute(&tags, "24.04").unwrap();
        assert_eq!(c.version_bump.as_deref(), Some("24.10"));
        assert!(c.moves.is_empty());

        assert_eq!(compute(&tags, "24.10").unwrap().version_bump, None);
    }
}
