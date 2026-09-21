//! Distro release ordering for base image variant suffixes.
//!
//! Only what is needed to answer "is this variant a newer release of the
//! same distro". An unrecognised suffix yields no release, which the
//! candidate logic reads as "offer no OS move" — version bumps still work.

/// A distro release, comparable only against the same family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    /// Distro family, e.g. `"debian"`. Ordinals are only comparable within one.
    pub family: &'static str,
    /// `major * 100 + minor`, so 24.04 sorts above 22.04 and Debian 13
    /// above Debian 12. Only meaningful within one family.
    pub ord: u32,
}

impl Release {
    /// Whether `self` is a later release of the same distro as `other`.
    ///
    /// Always false across families: Ubuntu 24.04 is not "newer than"
    /// Debian 12, it is a different operating system.
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.family == other.family && self.ord > other.ord
    }
}

/// Codenames cella knows. Deliberately incomplete: a missing entry costs
/// an OS-move suggestion, a wrong entry silently misorders releases.
///
/// Debian `forky` (14) is omitted because it is unreleased.
const CODENAMES: &[(&str, &str, u32)] = &[
    ("buster", "debian", 1000),
    ("bullseye", "debian", 1100),
    ("bookworm", "debian", 1200),
    ("trixie", "debian", 1300),
    ("focal", "ubuntu", 2004),
    ("jammy", "ubuntu", 2204),
    ("noble", "ubuntu", 2404),
    ("resolute", "ubuntu", 2604),
];

/// Families that also appear spelled out with a version number, in both
/// `debian-12` and `debian12` forms.
const FAMILIES: &[&str] = &["debian", "ubuntu", "alpine"];

/// Whether `variant` is a distro codename rather than a numeric spelling.
///
/// Used to pick one canonical spelling when a repository publishes several
/// aliases of the same release (`bookworm`, `debian-12`, `debian12`).
pub fn is_codename(variant: &str) -> bool {
    CODENAMES.iter().any(|(name, _, _)| *name == variant)
}

/// Map a variant suffix to a release, or `None` when cella does not
/// recognise it.
pub fn parse_variant(variant: &str) -> Option<Release> {
    if let Some(&(_, family, ord)) = CODENAMES.iter().find(|(name, _, _)| *name == variant) {
        return Some(Release { family, ord });
    }

    let family = FAMILIES.iter().find(|f| variant.starts_with(**f))?;
    let tail = &variant[family.len()..];
    let rest = tail.strip_prefix('-').unwrap_or(tail);
    if rest.is_empty() {
        // Bare `debian` / `ubuntu` / `alpine` float to the newest release.
        return None;
    }

    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    if parts.next().is_some() {
        return None;
    }

    Some(Release {
        family,
        ord: major * 100 + minor,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codenames_and_both_numeric_spellings() {
        assert_eq!(
            parse_variant("bookworm"),
            Some(Release {
                family: "debian",
                ord: 1200
            })
        );
        assert_eq!(
            parse_variant("trixie"),
            Some(Release {
                family: "debian",
                ord: 1300
            })
        );
        assert_eq!(
            parse_variant("debian-12"),
            Some(Release {
                family: "debian",
                ord: 1200
            })
        );
        assert_eq!(
            parse_variant("debian12"),
            Some(Release {
                family: "debian",
                ord: 1200
            })
        );
        assert_eq!(
            parse_variant("ubuntu-24.04"),
            Some(Release {
                family: "ubuntu",
                ord: 2404
            })
        );
        assert_eq!(
            parse_variant("ubuntu24.04"),
            Some(Release {
                family: "ubuntu",
                ord: 2404
            })
        );
        assert_eq!(
            parse_variant("noble"),
            Some(Release {
                family: "ubuntu",
                ord: 2404
            })
        );
        assert_eq!(
            parse_variant("resolute"),
            Some(Release {
                family: "ubuntu",
                ord: 2604
            })
        );
        assert_eq!(
            parse_variant("alpine3.20"),
            Some(Release {
                family: "alpine",
                ord: 320
            })
        );
    }

    #[test]
    fn unknown_variants_yield_no_release() {
        assert_eq!(parse_variant("forky"), None);
        assert_eq!(parse_variant("mycustomtag"), None);
        assert_eq!(parse_variant(""), None);
    }

    #[test]
    fn bare_family_names_are_floating_not_releases() {
        assert_eq!(parse_variant("debian"), None);
        assert_eq!(parse_variant("ubuntu"), None);
        assert_eq!(parse_variant("alpine"), None);
    }

    #[test]
    fn orders_only_within_a_family() {
        let bookworm = parse_variant("bookworm").unwrap();
        let trixie = parse_variant("trixie").unwrap();
        let noble = parse_variant("noble").unwrap();
        assert!(trixie.is_newer_than(&bookworm));
        assert!(!bookworm.is_newer_than(&trixie));
        assert!(
            !noble.is_newer_than(&bookworm),
            "cross-family comparison must never hold"
        );
    }

    #[test]
    fn codename_and_numeric_spellings_of_one_release_agree() {
        // `bookworm` and `debian-12` must be the same release, or an OS move
        // would be offered from a release to itself.
        assert_eq!(parse_variant("bookworm"), parse_variant("debian-12"));
        assert_eq!(parse_variant("noble"), parse_variant("ubuntu-24.04"));
        assert_eq!(parse_variant("resolute"), parse_variant("ubuntu26.04"));
    }
}
