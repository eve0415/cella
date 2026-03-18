//! Parsing and normalization of devcontainer feature reference strings.
//!
//! The `features` field in devcontainer.json maps reference strings to option
//! objects.  References come in several formats (OCI, GitHub shorthand,
//! tarball URL, local path, deprecated bare identifier) and must be preserved
//! as-is for diagnostics before being normalized into a fetchable target.

use std::path::{Path, PathBuf};

use crate::{FeatureError, FeatureWarning};

// ---------------------------------------------------------------------------
// Parsed reference -- preserves the original format
// ---------------------------------------------------------------------------

/// A parsed feature reference, preserving the exact format the user wrote.
///
/// Call [`FeatureRef::normalize`] to resolve this to a [`NormalizedRef`] that
/// can actually be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureRef {
    /// Fully-qualified OCI reference (e.g. `ghcr.io/devcontainers/features/go:1`).
    Oci {
        registry: String,
        repository: String,
        tag: String,
    },
    /// GitHub shorthand: `owner/repo/feature` (exactly 3 path components, no
    /// hostname).  Normalized to `ghcr.io/owner/repo/feature:latest`.
    GitHubShorthand {
        owner: String,
        repo: String,
        feature: String,
    },
    /// HTTP/HTTPS tarball URL.
    TarballUrl { url: String },
    /// Local filesystem path (starts with `./` or `../`).
    LocalPath { path: String },
    /// Deprecated bare feature identifier (e.g. `fish`, `maven`).
    Deprecated { key: String },
}

// ---------------------------------------------------------------------------
// Normalized (fetchable) reference
// ---------------------------------------------------------------------------

/// A normalized reference that points to a concrete fetchable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedRef {
    /// An OCI artifact to pull.
    OciTarget {
        registry: String,
        repository: String,
        tag: String,
    },
    /// An HTTP(S) tarball to download.
    HttpTarget { url: String },
    /// A local directory containing the feature.
    LocalTarget { absolute_path: PathBuf },
}

// ---------------------------------------------------------------------------
// Deprecated feature lookup table
// ---------------------------------------------------------------------------

/// Mapping from deprecated bare identifiers to their OCI equivalents.
///
/// The spec hardcodes these; the original devcontainer CLI also carries a
/// static table (see `supportedFeatures` in the reference implementation).
fn deprecated_lookup(key: &str) -> Option<&'static str> {
    match key {
        "fish" => Some("ghcr.io/devcontainers/features/fish:1"),
        "maven" | "gradle" => Some("ghcr.io/devcontainers/features/java:1"),
        "homebrew" => Some("ghcr.io/devcontainers/features/homebrew-package:1"),
        "jupyterlab" => Some("ghcr.io/devcontainers/features/python:1"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tag splitting helper
// ---------------------------------------------------------------------------

/// Split a tag from the end of a reference string.
///
/// Uses `rsplit_once(':')` but checks that what follows the colon is not a
/// port number (i.e. it must not contain `/`).  Returns `(base, tag)` where
/// tag defaults to `"latest"` when absent.
fn split_tag(s: &str) -> (&str, &str) {
    match s.rsplit_once(':') {
        Some((base, candidate)) if !candidate.contains('/') && !candidate.is_empty() => {
            (base, candidate)
        }
        _ => (s, "latest"),
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

impl FeatureRef {
    /// Parse a raw feature reference string from devcontainer.json.
    ///
    /// # Errors
    ///
    /// Returns [`FeatureError::InvalidReference`] when the string is
    /// syntactically invalid (e.g. 1-2 bare path components without a
    /// hostname, or an unknown deprecated identifier).
    ///
    /// # Panics
    ///
    /// Panics are not expected: internal `expect` calls are guarded by
    /// preceding checks that guarantee the invariant holds (e.g. the input
    /// contains `/` before `split_once('/')` is called).
    pub fn parse(raw: &str) -> Result<Self, FeatureError> {
        let input = raw.trim();

        if input.is_empty() {
            return Err(FeatureError::InvalidReference {
                reference: raw.to_owned(),
                reason: "empty feature reference".to_owned(),
            });
        }

        // Rule 1: local path
        if input.starts_with("./") || input.starts_with("../") {
            return Ok(Self::LocalPath {
                path: input.to_owned(),
            });
        }

        // Rule 2: tarball URL
        if input.starts_with("http://") || input.starts_with("https://") {
            return Ok(Self::TarballUrl {
                url: input.to_owned(),
            });
        }

        // Rule 6: bare identifier (no `/`) -- deprecated lookup
        if !input.contains('/') {
            // Strip a possible tag for the lookup, but keep it for the key.
            let (base, _tag) = split_tag(input);
            if deprecated_lookup(base).is_some() {
                return Ok(Self::Deprecated {
                    key: input.to_owned(),
                });
            }
            return Err(FeatureError::InvalidReference {
                reference: raw.to_owned(),
                reason: format!(
                    "bare identifier '{input}' is not a known deprecated feature; \
                     use a fully-qualified OCI reference instead"
                ),
            });
        }

        // Split off the tag before counting components.
        let (base, tag) = split_tag(input);

        // Rule 3: hostname detection -- first component contains a `.`
        let first_component = base.split('/').next().unwrap_or(base);
        let has_hostname = first_component.contains('.');

        if has_hostname {
            // OCI reference with an explicit registry.
            let (registry, repository) =
                base.split_once('/').expect("contains '/' -- checked above");
            return Ok(Self::Oci {
                registry: registry.to_owned(),
                repository: repository.to_owned(),
                tag: tag.to_owned(),
            });
        }

        // Check for port-style registry (e.g. `localhost:5000/repo/feature`)
        // where the first component contains `:` (which we already consumed
        // via `split_tag` on the full input).  Re-examine the raw input.
        if first_component.contains(':') {
            // The original `split_tag` may have split on the port colon.
            // Re-parse: the first component IS the registry (with port).
            let (registry, repository) = input
                .split_once('/')
                .expect("contains '/' -- checked above");
            let (repository, tag) = split_tag(repository);
            return Ok(Self::Oci {
                registry: registry.to_owned(),
                repository: repository.to_owned(),
                tag: tag.to_owned(),
            });
        }

        // Count path components (no hostname).
        let component_count = base.split('/').count();

        match component_count {
            // Rule 4: exactly 3 components → GitHub shorthand
            3 => {
                let mut parts = base.splitn(3, '/');
                let owner = parts.next().unwrap().to_owned();
                let repo = parts.next().unwrap().to_owned();
                let feature = parts.next().unwrap().to_owned();
                Ok(Self::GitHubShorthand {
                    owner,
                    repo,
                    feature,
                })
            }
            // Rule 5: 1-2 components without hostname → error
            1 | 2 => Err(FeatureError::InvalidReference {
                reference: raw.to_owned(),
                reason: format!(
                    "reference has {component_count} path component(s) but no hostname; \
                     use a fully-qualified OCI reference (e.g. ghcr.io/owner/repo/feature)"
                ),
            }),
            // 4+ components without hostname → also ambiguous, error
            _ => Err(FeatureError::InvalidReference {
                reference: raw.to_owned(),
                reason: format!(
                    "reference has {component_count} path components but no hostname; \
                     cannot determine registry"
                ),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

impl FeatureRef {
    /// Normalize the parsed reference into a fetchable target.
    ///
    /// For [`FeatureRef::Deprecated`] variants, a [`FeatureWarning`] is
    /// returned alongside the target so callers can surface deprecation
    /// notices to the user.
    ///
    /// `workspace_root` is needed to resolve relative local paths to absolute
    /// ones.
    ///
    /// # Errors
    ///
    /// Returns [`FeatureError::InvalidReference`] if a deprecated key has no
    /// known OCI mapping (should not happen if `parse` succeeded, but
    /// defensive).
    ///
    /// # Panics
    ///
    /// Panics are not expected: the `expect` in the `Deprecated` arm splits
    /// hardcoded OCI strings that are guaranteed to contain `/`.
    pub fn normalize(
        &self,
        workspace_root: &Path,
    ) -> Result<(NormalizedRef, Option<FeatureWarning>), FeatureError> {
        match self {
            Self::Oci {
                registry,
                repository,
                tag,
            } => Ok((
                NormalizedRef::OciTarget {
                    registry: registry.clone(),
                    repository: repository.clone(),
                    tag: tag.clone(),
                },
                None,
            )),

            Self::GitHubShorthand {
                owner,
                repo,
                feature,
            } => Ok((
                NormalizedRef::OciTarget {
                    registry: "ghcr.io".to_owned(),
                    repository: format!("{owner}/{repo}/{feature}"),
                    tag: "latest".to_owned(),
                },
                None,
            )),

            Self::TarballUrl { url } => Ok((NormalizedRef::HttpTarget { url: url.clone() }, None)),

            Self::LocalPath { path } => {
                let p = Path::new(path);
                let absolute = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    workspace_root.join(p)
                };
                Ok((
                    NormalizedRef::LocalTarget {
                        absolute_path: absolute,
                    },
                    None,
                ))
            }

            Self::Deprecated { key } => {
                let (lookup_key, _) = split_tag(key);
                let oci_ref = deprecated_lookup(lookup_key).ok_or_else(|| {
                    FeatureError::InvalidReference {
                        reference: key.clone(),
                        reason: format!("unknown deprecated feature '{lookup_key}'"),
                    }
                })?;

                let (base, tag) = split_tag(oci_ref);
                let (registry, repository) =
                    base.split_once('/').expect("hardcoded refs contain '/'");

                let warning = FeatureWarning::DeprecatedFeature {
                    key: key.clone(),
                    oci_equivalent: oci_ref.to_owned(),
                };

                Ok((
                    NormalizedRef::OciTarget {
                        registry: registry.to_owned(),
                        repository: repository.to_owned(),
                        tag: tag.to_owned(),
                    },
                    Some(warning),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl std::fmt::Display for FeatureRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oci {
                registry,
                repository,
                tag,
            } => write!(f, "{registry}/{repository}:{tag}"),
            Self::GitHubShorthand {
                owner,
                repo,
                feature,
            } => write!(f, "{owner}/{repo}/{feature}"),
            Self::TarballUrl { url } => write!(f, "{url}"),
            Self::LocalPath { path } => write!(f, "{path}"),
            Self::Deprecated { key } => write!(f, "{key}"),
        }
    }
}

impl std::fmt::Display for NormalizedRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OciTarget {
                registry,
                repository,
                tag,
            } => write!(f, "{registry}/{repository}:{tag}"),
            Self::HttpTarget { url } => write!(f, "{url}"),
            Self::LocalTarget { absolute_path } => write!(f, "{}", absolute_path.display()),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    // -----------------------------------------------------------------------
    // split_tag helper
    // -----------------------------------------------------------------------

    #[test]
    fn split_tag_with_tag() {
        assert_eq!(
            split_tag("ghcr.io/owner/repo:v1"),
            ("ghcr.io/owner/repo", "v1")
        );
    }

    #[test]
    fn split_tag_without_tag() {
        assert_eq!(
            split_tag("ghcr.io/owner/repo"),
            ("ghcr.io/owner/repo", "latest")
        );
    }

    #[test]
    fn split_tag_port_not_tag() {
        // `localhost:5000/repo` -- the `:5000` is a port, not a tag
        assert_eq!(
            split_tag("localhost:5000/repo"),
            ("localhost:5000/repo", "latest")
        );
    }

    #[test]
    fn split_tag_port_and_tag() {
        assert_eq!(
            split_tag("localhost:5000/repo:v2"),
            ("localhost:5000/repo", "v2")
        );
    }

    #[test]
    fn split_tag_dotted_tag() {
        assert_eq!(
            split_tag("ghcr.io/owner/repo:1.2.3"),
            ("ghcr.io/owner/repo", "1.2.3")
        );
    }

    // -----------------------------------------------------------------------
    // Rule 1: local path
    // -----------------------------------------------------------------------

    #[test]
    fn parse_local_relative_dot() {
        let r = FeatureRef::parse("./my-feature").unwrap();
        assert_eq!(
            r,
            FeatureRef::LocalPath {
                path: "./my-feature".to_owned()
            }
        );
    }

    #[test]
    fn parse_local_relative_dotdot() {
        let r = FeatureRef::parse("../shared/feat").unwrap();
        assert_eq!(
            r,
            FeatureRef::LocalPath {
                path: "../shared/feat".to_owned()
            }
        );
    }

    // -----------------------------------------------------------------------
    // Rule 2: tarball URL
    // -----------------------------------------------------------------------

    #[test]
    fn parse_https_tarball() {
        let url = "https://example.com/features/go.tgz";
        let r = FeatureRef::parse(url).unwrap();
        assert_eq!(
            r,
            FeatureRef::TarballUrl {
                url: url.to_owned()
            }
        );
    }

    #[test]
    fn parse_http_tarball() {
        let url = "http://internal.corp/features/node.tgz";
        let r = FeatureRef::parse(url).unwrap();
        assert_eq!(
            r,
            FeatureRef::TarballUrl {
                url: url.to_owned()
            }
        );
    }

    // -----------------------------------------------------------------------
    // Rule 3: OCI reference (hostname contains `.`)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_oci_ghcr() {
        let r = FeatureRef::parse("ghcr.io/devcontainers/features/go:1.2").unwrap();
        assert_eq!(
            r,
            FeatureRef::Oci {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/go".to_owned(),
                tag: "1.2".to_owned(),
            }
        );
    }

    #[test]
    fn parse_oci_no_tag() {
        let r = FeatureRef::parse("ghcr.io/devcontainers/features/go").unwrap();
        assert_eq!(
            r,
            FeatureRef::Oci {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/go".to_owned(),
                tag: "latest".to_owned(),
            }
        );
    }

    #[test]
    fn parse_oci_custom_registry() {
        let r = FeatureRef::parse("myregistry.azurecr.io/team/features/custom:3").unwrap();
        assert_eq!(
            r,
            FeatureRef::Oci {
                registry: "myregistry.azurecr.io".to_owned(),
                repository: "team/features/custom".to_owned(),
                tag: "3".to_owned(),
            }
        );
    }

    #[test]
    fn parse_oci_localhost_with_port() {
        let r = FeatureRef::parse("localhost:5000/myfeatures/go:dev").unwrap();
        assert_eq!(
            r,
            FeatureRef::Oci {
                registry: "localhost:5000".to_owned(),
                repository: "myfeatures/go".to_owned(),
                tag: "dev".to_owned(),
            }
        );
    }

    #[test]
    fn parse_oci_localhost_port_no_tag() {
        let r = FeatureRef::parse("localhost:5000/myfeatures/go").unwrap();
        assert_eq!(
            r,
            FeatureRef::Oci {
                registry: "localhost:5000".to_owned(),
                repository: "myfeatures/go".to_owned(),
                tag: "latest".to_owned(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // Rule 4: GitHub shorthand (3 components, no hostname)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_github_shorthand() {
        let r = FeatureRef::parse("devcontainers/features/go").unwrap();
        assert_eq!(
            r,
            FeatureRef::GitHubShorthand {
                owner: "devcontainers".to_owned(),
                repo: "features".to_owned(),
                feature: "go".to_owned(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // Rule 5: 1-2 components → error
    // -----------------------------------------------------------------------

    #[test]
    fn parse_two_components_no_hostname_is_error() {
        let err = FeatureRef::parse("owner/repo").unwrap_err();
        assert!(matches!(err, FeatureError::InvalidReference { .. }));
    }

    #[test]
    fn parse_single_component_unknown_is_error() {
        let err = FeatureRef::parse("unknownthing").unwrap_err();
        assert!(matches!(err, FeatureError::InvalidReference { .. }));
    }

    // -----------------------------------------------------------------------
    // Rule 6: deprecated bare identifier
    // -----------------------------------------------------------------------

    #[test]
    fn parse_deprecated_fish() {
        let r = FeatureRef::parse("fish").unwrap();
        assert_eq!(
            r,
            FeatureRef::Deprecated {
                key: "fish".to_owned()
            }
        );
    }

    #[test]
    fn parse_deprecated_maven() {
        let r = FeatureRef::parse("maven").unwrap();
        assert_eq!(
            r,
            FeatureRef::Deprecated {
                key: "maven".to_owned()
            }
        );
    }

    #[test]
    fn parse_deprecated_gradle() {
        let r = FeatureRef::parse("gradle").unwrap();
        assert_eq!(
            r,
            FeatureRef::Deprecated {
                key: "gradle".to_owned()
            }
        );
    }

    #[test]
    fn parse_deprecated_homebrew() {
        let r = FeatureRef::parse("homebrew").unwrap();
        assert_eq!(
            r,
            FeatureRef::Deprecated {
                key: "homebrew".to_owned()
            }
        );
    }

    #[test]
    fn parse_deprecated_jupyterlab() {
        let r = FeatureRef::parse("jupyterlab").unwrap();
        assert_eq!(
            r,
            FeatureRef::Deprecated {
                key: "jupyterlab".to_owned()
            }
        );
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_empty_is_error() {
        let err = FeatureRef::parse("").unwrap_err();
        assert!(matches!(err, FeatureError::InvalidReference { .. }));
    }

    #[test]
    fn parse_whitespace_trimmed() {
        let r = FeatureRef::parse("  ghcr.io/devcontainers/features/go:1  ").unwrap();
        assert_eq!(
            r,
            FeatureRef::Oci {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/go".to_owned(),
                tag: "1".to_owned(),
            }
        );
    }

    #[test]
    fn parse_four_components_no_hostname_is_error() {
        let err = FeatureRef::parse("a/b/c/d").unwrap_err();
        assert!(matches!(err, FeatureError::InvalidReference { .. }));
    }

    // -----------------------------------------------------------------------
    // Normalization
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_oci() {
        let r = FeatureRef::Oci {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/go".to_owned(),
            tag: "1".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::OciTarget {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/go".to_owned(),
                tag: "1".to_owned(),
            }
        );
        assert!(warning.is_none());
    }

    #[test]
    fn normalize_github_shorthand() {
        let r = FeatureRef::GitHubShorthand {
            owner: "devcontainers".to_owned(),
            repo: "features".to_owned(),
            feature: "go".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::OciTarget {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/go".to_owned(),
                tag: "latest".to_owned(),
            }
        );
        assert!(warning.is_none());
    }

    #[test]
    fn normalize_tarball_url() {
        let r = FeatureRef::TarballUrl {
            url: "https://example.com/feat.tgz".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::HttpTarget {
                url: "https://example.com/feat.tgz".to_owned()
            }
        );
        assert!(warning.is_none());
    }

    #[test]
    fn normalize_local_path() {
        let r = FeatureRef::LocalPath {
            path: "./my-feature".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::LocalTarget {
                absolute_path: PathBuf::from("/workspace/./my-feature")
            }
        );
        assert!(warning.is_none());
    }

    #[test]
    fn normalize_deprecated_fish_emits_warning() {
        let r = FeatureRef::Deprecated {
            key: "fish".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::OciTarget {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/fish".to_owned(),
                tag: "1".to_owned(),
            }
        );
        assert!(warning.is_some());
        let w = warning.unwrap();
        match w {
            FeatureWarning::DeprecatedFeature {
                key,
                oci_equivalent,
            } => {
                assert_eq!(key, "fish");
                assert_eq!(oci_equivalent, "ghcr.io/devcontainers/features/fish:1");
            }
            _ => panic!("expected DeprecatedFeature warning"),
        }
    }

    #[test]
    fn normalize_deprecated_maven_emits_warning() {
        let r = FeatureRef::Deprecated {
            key: "maven".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::OciTarget {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/java".to_owned(),
                tag: "1".to_owned(),
            }
        );
        let w = warning.unwrap();
        assert!(matches!(w, FeatureWarning::DeprecatedFeature { .. }));
    }

    #[test]
    fn normalize_deprecated_homebrew_emits_warning() {
        let r = FeatureRef::Deprecated {
            key: "homebrew".to_owned(),
        };
        let (norm, warning) = r.normalize(Path::new("/workspace")).unwrap();
        assert_eq!(
            norm,
            NormalizedRef::OciTarget {
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features/homebrew-package".to_owned(),
                tag: "1".to_owned(),
            }
        );
        assert!(warning.is_some());
    }

    // -----------------------------------------------------------------------
    // Display
    // -----------------------------------------------------------------------

    #[test]
    fn display_oci() {
        let r = FeatureRef::Oci {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/go".to_owned(),
            tag: "1".to_owned(),
        };
        assert_eq!(r.to_string(), "ghcr.io/devcontainers/features/go:1");
    }

    #[test]
    fn display_github_shorthand() {
        let r = FeatureRef::GitHubShorthand {
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            feature: "feat".to_owned(),
        };
        assert_eq!(r.to_string(), "owner/repo/feat");
    }

    #[test]
    fn display_normalized_oci() {
        let n = NormalizedRef::OciTarget {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/go".to_owned(),
            tag: "1".to_owned(),
        };
        assert_eq!(n.to_string(), "ghcr.io/devcontainers/features/go:1");
    }
}
