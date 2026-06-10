//! Types and naming rules for cella-managed container networks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Label that marks networks created by cella.
pub const MANAGED_LABEL: &str = "dev.cella.managed";
/// Label value required on [`MANAGED_LABEL`] for cella-managed networks.
pub const MANAGED_VALUE: &str = "true";
/// Label that carries the workspace path on per-repo networks.
pub const REPO_LABEL: &str = "dev.cella.repo";

/// Shared network name for cella containers (cross-container DNS hub).
pub const CELLA_NETWORK_NAME: &str = "cella";

/// Derive a per-repository network name from a repo path.
///
/// Returns `cella-net-{first 12 hex chars of SHA-256 of repo_path}`.
#[must_use]
pub fn repo_network_name(repo_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_path.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    let short = hex::encode(&hash[..6]); // 6 bytes = 12 hex chars
    format!("cella-net-{short}")
}

/// Canonicalize a path for network identity, falling back to the
/// original path if canonicalization fails (e.g. path doesn't exist).
///
/// Must be applied consistently at every network-op boundary so that
/// `cella down --rm` and `cella prune` hash to the same name the
/// container was created with. `container_labels` already canonicalizes
/// the `dev.cella.workspace_path` label, so this keeps network names
/// aligned with that source of truth.
#[must_use]
pub fn canonicalize_for_network(repo_path: &Path) -> PathBuf {
    repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf())
}

/// Derive the workspace network name the same way `cella up` does, so
/// `cella down --rm` / `cella prune` target the matching network.
#[must_use]
pub fn workspace_network_name(workspace_root: &Path) -> String {
    repo_network_name(&canonicalize_for_network(workspace_root))
}

/// A cella-managed Docker network, as returned by
/// [`ContainerBackend::list_managed_networks`](crate::ContainerBackend::list_managed_networks).
///
/// "Managed" means the network carries the `dev.cella.managed=true`
/// label. cella creates both a shared `cella` network (cross-container
/// DNS hub) and per-workspace `cella-net-{hash}` networks; both are
/// reported here uniformly.
#[derive(Debug, Clone)]
pub struct ManagedNetwork {
    /// Network name (e.g. `cella` or `cella-net-abcdef123456`).
    pub name: String,
    /// Value of the `dev.cella.repo` label, if set. Only per-repo
    /// networks carry this.
    pub repo_path: Option<String>,
    /// Number of attached container endpoints. Includes stopped
    /// containers whose endpoints haven't been cleaned up.
    pub container_count: usize,
    /// Creation timestamp in RFC 3339 format, if available.
    pub created_at: Option<String>,
    /// Full label map. Callers that only need `dev.cella.repo` should
    /// use [`Self::repo_path`].
    pub labels: HashMap<String, String>,
}

/// Outcome of a single network-removal attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    /// The network was removed.
    Removed,
    /// The network had attached container endpoints; left in place.
    SkippedInUse,
    /// The network does not exist (either never existed or was removed
    /// by a concurrent caller).
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cella_network_name_constant() {
        assert_eq!(CELLA_NETWORK_NAME, "cella");
    }

    #[test]
    fn cella_network_name_is_valid_network_name() {
        // Network names must be lowercase alphanumeric.
        assert!(
            CELLA_NETWORK_NAME.chars().all(|c| c.is_ascii_lowercase()),
            "network name should be all lowercase"
        );
    }

    #[test]
    fn repo_network_name_deterministic() {
        let path = Path::new("/home/user/my-project");
        let name1 = repo_network_name(path);
        let name2 = repo_network_name(path);
        assert_eq!(name1, name2);
    }

    #[test]
    fn repo_network_name_has_prefix() {
        let name = repo_network_name(Path::new("/any/path"));
        assert!(
            name.starts_with("cella-net-"),
            "name should start with 'cella-net-': {name}"
        );
    }

    #[test]
    fn repo_network_name_has_12_hex_chars() {
        let name = repo_network_name(Path::new("/some/repo"));
        let hash_part = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(
            hash_part.len(),
            12,
            "hash portion should be 12 hex chars: {hash_part}"
        );
        assert!(
            hash_part.chars().all(|c| c.is_ascii_hexdigit()),
            "hash portion should be hex: {hash_part}"
        );
    }

    #[test]
    fn repo_network_name_different_paths_differ() {
        let name1 = repo_network_name(Path::new("/project/a"));
        let name2 = repo_network_name(Path::new("/project/b"));
        assert_ne!(name1, name2);
    }

    #[test]
    fn repo_network_name_empty_path() {
        // Edge case: empty path should still produce a valid name
        let name = repo_network_name(Path::new(""));
        assert!(name.starts_with("cella-net-"));
        let hash_part = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(hash_part.len(), 12);
    }

    #[test]
    fn repo_network_name_with_unicode_path() {
        let name = repo_network_name(Path::new("/home/user/proyecto-espanol"));
        assert!(name.starts_with("cella-net-"));
        let hash = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_network_name_with_spaces() {
        let name = repo_network_name(Path::new("/home/user/my project"));
        assert!(name.starts_with("cella-net-"));
        let hash = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(hash.len(), 12);
    }

    #[test]
    fn repo_network_name_with_dots() {
        let name = repo_network_name(Path::new("/home/user/.hidden-project"));
        assert!(name.starts_with("cella-net-"));
    }

    #[test]
    fn repo_network_name_very_long_path() {
        let long_path = "/".to_string() + &"a".repeat(1000);
        let name = repo_network_name(Path::new(&long_path));
        assert!(name.starts_with("cella-net-"));
        let hash = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(
            hash.len(),
            12,
            "hash should always be 12 chars regardless of path length"
        );
    }

    #[test]
    fn repo_network_name_root_path() {
        let name = repo_network_name(Path::new("/"));
        assert!(name.starts_with("cella-net-"));
    }

    #[test]
    fn repo_network_name_similar_paths_differ() {
        let name1 = repo_network_name(Path::new("/project/a"));
        let name2 = repo_network_name(Path::new("/project/A"));
        assert_ne!(
            name1, name2,
            "case-different paths should produce different hashes"
        );
    }

    #[test]
    fn workspace_network_name_canonicalizes_then_hashes() {
        let tmpdir = std::env::temp_dir();
        let canonical = canonicalize_for_network(&tmpdir);
        assert_eq!(
            workspace_network_name(&tmpdir),
            repo_network_name(&canonical)
        );
    }
}
