//! Devcontainer feature lockfile support.
//!
//! Implements reading, writing, generating, and comparing the
//! `devcontainer-lock.json` / `.devcontainer-lock.json` file used to pin
//! OCI feature digests across builds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::FeatureError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single feature entry inside the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockfileEntry {
    /// Resolved version (OCI tag, e.g. `"1"` or `"latest"`).
    pub version: String,
    /// Full resolved reference with manifest digest, e.g.
    /// `"ghcr.io/devcontainers/features/node@sha256:abc..."`.
    pub resolved: String,
    /// Manifest digest, e.g. `"sha256:abc..."`.
    pub integrity: String,
    /// Keys of other features in the lockfile this one depends on.
    #[serde(rename = "dependsOn", skip_serializing_if = "Vec::is_empty", default)]
    pub depends_on: Vec<String>,
}

/// The contents of a `devcontainer-lock.json` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Lockfile {
    /// Map from feature key (OCI ref without version suffix) to entry.
    ///
    /// Uses [`BTreeMap`] so keys are always serialized in alphabetical order.
    pub features: BTreeMap<String, LockfileEntry>,
}

/// Errors specific to lockfile validation.
#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
    /// The lockfile does not exist on disk.
    #[error("Lockfile does not exist.")]
    Missing,
    /// The generated lockfile does not match the one on disk.
    #[error("Lockfile does not match.")]
    Mismatch,
}

/// Controls how the lockfile is read and written during feature resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LockfilePolicy {
    /// Write/update the lockfile after resolution (default).
    #[default]
    Update,
    /// Skip reading and writing the lockfile entirely.
    NoLockfile,
    /// Require the lockfile to match; fail if missing or different.
    Frozen,
    /// Resolve fresh (ignoring any locked digests) and return the regenerated
    /// lockfile WITHOUT writing it — the `upgrade` command writes or prints it
    /// itself so `--dry-run` never touches disk.
    Upgrade,
}

// ---------------------------------------------------------------------------
// Path derivation
// ---------------------------------------------------------------------------

/// Derive the lockfile path from the devcontainer config path.
///
/// If the config filename starts with `'.'` (e.g. `.devcontainer.json`) the
/// lockfile is `.devcontainer-lock.json`; otherwise it is
/// `devcontainer-lock.json`. The lockfile is always placed in the **same
/// directory** as the config.
#[must_use]
pub fn lockfile_path(config_path: &Path) -> PathBuf {
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let filename = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("devcontainer.json");

    let lock_name = if filename.starts_with('.') {
        ".devcontainer-lock.json"
    } else {
        "devcontainer-lock.json"
    };

    dir.join(lock_name)
}

// ---------------------------------------------------------------------------
// Read / write
// ---------------------------------------------------------------------------

/// Read and deserialize a lockfile.  Returns `None` on any error (missing,
/// I/O failure, malformed JSON).
#[must_use]
pub fn read_lockfile(config_path: &Path) -> Option<Lockfile> {
    let path = lockfile_path(config_path);
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Serialize and write a lockfile to disk, with a trailing newline.
///
/// # Errors
///
/// Returns [`FeatureError::Io`] on I/O failure.
pub fn write_lockfile(config_path: &Path, lockfile: &Lockfile) -> Result<(), FeatureError> {
    let path = lockfile_path(config_path);
    let mut json = serde_json::to_string_pretty(lockfile)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    json.push('\n');
    std::fs::write(path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Generate a [`Lockfile`] from a list of resolved OCI features.
///
/// Each tuple is `(key, version, resolved_full, integrity, depends_on)` where:
/// - `key` — feature ID with any `:version` / `@digest` suffix stripped
/// - `version` — the OCI tag that was fetched
/// - `resolved_full` — `"registry/repository@digest"`
/// - `integrity` — the manifest digest (`"sha256:..."`)
/// - `depends_on` — keys of features this one depends on (may be empty)
#[must_use]
pub fn generate_lockfile(
    resolved_oci_features: &[(String, String, String, String, Vec<String>)],
) -> Lockfile {
    let mut features = BTreeMap::new();
    for (key, version, resolved, integrity, depends_on) in resolved_oci_features {
        features.insert(
            key.clone(),
            LockfileEntry {
                version: version.clone(),
                resolved: resolved.clone(),
                integrity: integrity.clone(),
                depends_on: depends_on.clone(),
            },
        );
    }
    Lockfile { features }
}

/// Strip the `:version` suffix from a raw feature reference string.
///
/// `"ghcr.io/devcontainers/features/git:1"` → `"ghcr.io/devcontainers/features/git"`
///
/// Only strips the trailing component when it contains no `'/'` (i.e. it
/// looks like a tag, not a registry path segment).
#[must_use]
pub fn lockfile_key(raw_ref: &str) -> String {
    if let Some(pos) = raw_ref.rfind(':') {
        let suffix = &raw_ref[pos + 1..];
        if !suffix.contains('/') {
            return raw_ref[..pos].to_string();
        }
    }
    raw_ref.to_string()
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Compare an existing lockfile against a freshly-generated one.
///
/// Returns `Ok(())` when they match, or [`LockfileError::Mismatch`] when they
/// differ. The missing-file case is handled by the caller before this point
/// (a `read_lockfile` returning `None`), so this only compares structure.
///
/// # Errors
///
/// Returns [`LockfileError::Mismatch`] when `existing` differs from `generated`.
pub fn compare_lockfile(existing: &Lockfile, generated: &Lockfile) -> Result<(), LockfileError> {
    if existing == generated {
        Ok(())
    } else {
        Err(LockfileError::Mismatch)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // lockfile_path
    // -----------------------------------------------------------------------

    #[test]
    fn lockfile_path_devcontainer_dir() {
        let config = Path::new("/workspace/.devcontainer/devcontainer.json");
        let lock = lockfile_path(config);
        assert_eq!(
            lock,
            Path::new("/workspace/.devcontainer/devcontainer-lock.json")
        );
    }

    #[test]
    fn lockfile_path_root_dotfile() {
        let config = Path::new("/workspace/.devcontainer.json");
        let lock = lockfile_path(config);
        assert_eq!(lock, Path::new("/workspace/.devcontainer-lock.json"));
    }

    #[test]
    fn lockfile_path_named_config() {
        let config = Path::new("/workspace/.devcontainer/my.json");
        let lock = lockfile_path(config);
        assert_eq!(
            lock,
            Path::new("/workspace/.devcontainer/devcontainer-lock.json")
        );
    }

    // -----------------------------------------------------------------------
    // serde round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn serde_round_trip_sorted_keys() {
        let mut features = BTreeMap::new();
        features.insert(
            "ghcr.io/devcontainers/features/node".to_string(),
            LockfileEntry {
                version: "1".to_string(),
                resolved: "ghcr.io/devcontainers/features/node@sha256:aabbcc".to_string(),
                integrity: "sha256:aabbcc".to_string(),
                depends_on: vec![],
            },
        );
        features.insert(
            "ghcr.io/devcontainers/features/git".to_string(),
            LockfileEntry {
                version: "1".to_string(),
                resolved: "ghcr.io/devcontainers/features/git@sha256:112233".to_string(),
                integrity: "sha256:112233".to_string(),
                depends_on: vec![],
            },
        );
        let lockfile = Lockfile { features };

        let json = serde_json::to_string_pretty(&lockfile).unwrap();

        // Keys must be alphabetically sorted (BTreeMap guarantees this).
        let git_pos = json.find("features/git").unwrap();
        let node_pos = json.find("features/node").unwrap();
        assert!(git_pos < node_pos, "keys must be sorted: git before node");

        // Trailing newline after write.
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("devcontainer.json");
        write_lockfile(&config, &lockfile).unwrap();
        let raw = std::fs::read_to_string(lockfile_path(&config)).unwrap();
        assert!(
            raw.ends_with('\n'),
            "written lockfile must end with newline"
        );

        // Round-trip.
        let recovered: Lockfile = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered, lockfile);
    }

    #[test]
    fn depends_on_omitted_when_empty() {
        let entry = LockfileEntry {
            version: "1".to_string(),
            resolved: "ghcr.io/x/y@sha256:00".to_string(),
            integrity: "sha256:00".to_string(),
            depends_on: vec![],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("depends_on"),
            "empty depends_on must be omitted"
        );
        assert!(
            !json.contains("dependsOn"),
            "empty depends_on must be omitted"
        );
    }

    #[test]
    fn depends_on_present_when_non_empty() {
        let entry = LockfileEntry {
            version: "1".to_string(),
            resolved: "ghcr.io/x/y@sha256:00".to_string(),
            integrity: "sha256:00".to_string(),
            depends_on: vec!["ghcr.io/x/z".to_string()],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains("dependsOn"),
            "non-empty depends_on must serialize as the camelCase `dependsOn` key"
        );
        assert!(
            !json.contains("depends_on"),
            "must not leak the snake_case field name"
        );
    }

    // -----------------------------------------------------------------------
    // generate_lockfile
    // -----------------------------------------------------------------------

    #[test]
    fn generate_lockfile_from_synthetic_data() {
        let data = vec![(
            "ghcr.io/devcontainers/features/git".to_string(),
            "1".to_string(),
            "ghcr.io/devcontainers/features/git@sha256:deadbeef".to_string(),
            "sha256:deadbeef".to_string(),
            vec![],
        )];
        let lf = generate_lockfile(&data);
        assert_eq!(lf.features.len(), 1);
        let entry = &lf.features["ghcr.io/devcontainers/features/git"];
        assert_eq!(entry.version, "1");
        assert_eq!(entry.integrity, "sha256:deadbeef");
        assert!(entry.depends_on.is_empty());
    }

    // -----------------------------------------------------------------------
    // lockfile_key
    // -----------------------------------------------------------------------

    #[test]
    fn lockfile_key_strips_version() {
        assert_eq!(
            lockfile_key("ghcr.io/devcontainers/features/git:1"),
            "ghcr.io/devcontainers/features/git"
        );
    }

    #[test]
    fn lockfile_key_no_version() {
        assert_eq!(
            lockfile_key("ghcr.io/devcontainers/features/git"),
            "ghcr.io/devcontainers/features/git"
        );
    }

    #[test]
    fn lockfile_key_port_not_stripped() {
        assert_eq!(
            lockfile_key("localhost:5000/features/git"),
            "localhost:5000/features/git"
        );
    }

    // -----------------------------------------------------------------------
    // compare_lockfile
    // -----------------------------------------------------------------------

    #[test]
    fn compare_matching_lockfiles_ok() {
        let lf = generate_lockfile(&[(
            "ghcr.io/x/y".to_string(),
            "1".to_string(),
            "ghcr.io/x/y@sha256:aa".to_string(),
            "sha256:aa".to_string(),
            vec![],
        )]);
        assert!(compare_lockfile(&lf, &lf).is_ok());
    }

    #[test]
    fn compare_mismatch_returns_error() {
        let lf1 = generate_lockfile(&[(
            "ghcr.io/x/y".to_string(),
            "1".to_string(),
            "ghcr.io/x/y@sha256:aa".to_string(),
            "sha256:aa".to_string(),
            vec![],
        )]);
        let lf2 = generate_lockfile(&[(
            "ghcr.io/x/y".to_string(),
            "1".to_string(),
            "ghcr.io/x/y@sha256:bb".to_string(),
            "sha256:bb".to_string(),
            vec![],
        )]);
        assert!(matches!(
            compare_lockfile(&lf1, &lf2),
            Err(LockfileError::Mismatch)
        ));
    }

    #[test]
    fn compare_empty_present_lockfile_against_nonempty_is_mismatch() {
        // A present-but-empty lockfile differs from what resolution produced:
        // that is a Mismatch, not Missing (Missing is only for an absent file,
        // handled by the caller before compare_lockfile is reached).
        let empty = Lockfile::default();
        let generated = generate_lockfile(&[(
            "ghcr.io/x/y".to_string(),
            "1".to_string(),
            "ghcr.io/x/y@sha256:aa".to_string(),
            "sha256:aa".to_string(),
            vec![],
        )]);
        assert!(matches!(
            compare_lockfile(&empty, &generated),
            Err(LockfileError::Mismatch)
        ));
    }

    #[test]
    fn compare_two_empty_lockfiles_matches() {
        let empty = Lockfile::default();
        assert!(compare_lockfile(&empty, &Lockfile::default()).is_ok());
    }
}
