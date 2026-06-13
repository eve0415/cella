//! Persistent on-disk cache for downloaded devcontainer features.
//!
//! OCI features are stored by registry/repository/digest.  HTTP tarball
//! features are stored under a truncated SHA-256 of the URL.  Build
//! contexts live under a separate `builds/` subtree keyed by config hash.
//!
//! All writes go through a staging directory (`{name}.partial-{pid}`) and
//! are committed with an atomic `rename()`.  This makes concurrent fetches
//! of the same feature safe: the first process to finish wins, and later
//! renames simply overwrite the (identical) directory.

use std::io;
use std::path::{Path, PathBuf};
use std::process;

use sha2::{Digest, Sha256};

/// On-disk feature cache rooted at a platform-appropriate location.
#[derive(Debug, Clone)]
pub struct FeatureCache {
    root: PathBuf,
}

impl FeatureCache {
    /// Create a cache rooted at the platform default location.
    ///
    /// Uses `dirs::cache_dir()/cella/features/` when available, falling
    /// back to `/tmp/cella-features-cache` when the platform cache
    /// directory cannot be determined.
    pub fn new() -> Self {
        let root = dirs::cache_dir().map_or_else(
            || PathBuf::from("/tmp/cella-features-cache"),
            |d| d.join("cella").join("features"),
        );
        Self { root }
    }

    /// Create a cache rooted at an explicit path (useful for testing).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Check whether an OCI feature layer is already cached.
    ///
    /// Returns `Some(path)` when the directory exists, `None` otherwise.
    pub fn get_oci(&self, registry: &str, repository: &str, digest: &str) -> Option<PathBuf> {
        let path = self.oci_path(registry, repository, digest);
        path.is_dir().then_some(path)
    }

    /// Compute the cache path for an OCI feature layer.
    ///
    /// Each ref component is hashed before being used as a path segment so
    /// that crafted values containing `..`, absolute paths, or embedded
    /// separators cannot escape the cache root.  This changes the on-disk
    /// layout; any existing unhashed entries are treated as misses.
    pub fn oci_path(&self, registry: &str, repository: &str, digest: &str) -> PathBuf {
        self.root
            .join("oci")
            .join(hash_ref_component(registry))
            .join(hash_ref_component(repository))
            .join(hash_ref_component(digest))
    }

    /// Check whether a URL-fetched feature tarball is already cached.
    ///
    /// Returns `Some(path)` when the directory exists, `None` otherwise.
    pub fn get_url(&self, url: &str) -> Option<PathBuf> {
        let path = self.url_path(url);
        path.is_dir().then_some(path)
    }

    /// Compute the cache path for a URL-fetched feature.
    ///
    /// The path is derived from the first 16 hex characters of the
    /// SHA-256 digest of the URL.
    pub fn url_path(&self, url: &str) -> PathBuf {
        let hash = hex::encode(Sha256::digest(url.as_bytes()));
        let short = &hash[..16];
        self.root.join("urls").join(short)
    }

    /// Compute the path for a build context identified by `config_hash`.
    pub fn build_context_path(&self, config_hash: &str) -> PathBuf {
        // Build contexts live one level up from the features root,
        // alongside it under the cella cache directory.
        self.root
            .parent()
            .unwrap_or(&self.root)
            .join("builds")
            .join(config_hash)
    }

    /// Return a staging path for the given final path.
    ///
    /// The staging name is `{final_name}.partial-{pid}` placed alongside
    /// the final path.  The caller should write into this directory, then
    /// call [`commit`](Self::commit) to atomically swap it into place.
    pub fn staging_path(final_path: &Path) -> PathBuf {
        let name = final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging_name = format!("{name}.partial-{}", process::id());
        final_path.with_file_name(staging_name)
    }

    /// Atomically commit a staging directory to its final location.
    ///
    /// Uses `fs::rename` which is atomic on POSIX.  If the final path
    /// already exists (another process won the race), the staging
    /// directory is removed and the existing entry is kept.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if both the rename and the cleanup of the
    /// staging directory fail.
    pub fn commit(staging: &Path, final_path: &Path) -> io::Result<()> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(staging, final_path) {
            Ok(()) => Ok(()),
            Err(_) if final_path.exists() => {
                // Another process won the race.  Clean up our staging dir.
                let _ = std::fs::remove_dir_all(staging);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Default for FeatureCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash an untrusted OCI ref component (registry, repository, or digest) to a
/// fixed-length hex string safe for use as a single filesystem path segment.
///
/// SHA-256 over the raw bytes produces a 64-character hex string that contains
/// no path separators, no `..` sequences, and no leading dots, regardless of
/// the input.  This is the same approach used by [`FeatureCache::url_path`]
/// and by the collection-index path helpers.
fn hash_ref_component(component: &str) -> String {
    hex::encode(Sha256::digest(component.as_bytes()))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // get_oci: miss and hit
    // -----------------------------------------------------------------------

    #[test]
    fn cache_miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        assert!(
            cache
                .get_oci("ghcr.io", "devcontainers/features/node", "sha256-abc")
                .is_none()
        );
    }

    #[test]
    fn cache_hit_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("ghcr.io", "devcontainers/features/node", "sha256-abc");
        std::fs::create_dir_all(&path).unwrap();
        let hit = cache.get_oci("ghcr.io", "devcontainers/features/node", "sha256-abc");
        assert_eq!(hit, Some(path));
    }

    // -----------------------------------------------------------------------
    // url_path: determinism and collision resistance
    // -----------------------------------------------------------------------

    #[test]
    fn url_path_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let a = cache.url_path("https://example.com/feature.tgz");
        let b = cache.url_path("https://example.com/feature.tgz");
        assert_eq!(a, b);
    }

    #[test]
    fn different_urls_produce_different_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let a = cache.url_path("https://example.com/feature-a.tgz");
        let b = cache.url_path("https://example.com/feature-b.tgz");
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // get_url: miss and hit
    // -----------------------------------------------------------------------

    #[test]
    fn url_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        assert!(cache.get_url("https://example.com/feat.tgz").is_none());
    }

    #[test]
    fn url_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.url_path("https://example.com/feat.tgz");
        std::fs::create_dir_all(&path).unwrap();
        assert_eq!(cache.get_url("https://example.com/feat.tgz"), Some(path));
    }

    // -----------------------------------------------------------------------
    // staging_path
    // -----------------------------------------------------------------------

    #[test]
    fn staging_path_includes_pid() {
        let final_path = PathBuf::from("/cache/oci/ghcr.io/features/node/sha256-abc");
        let staging = FeatureCache::staging_path(&final_path);
        let name = staging.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains(&format!(".partial-{}", process::id())),
            "staging name should include PID: {name}",
        );
    }

    #[test]
    fn staging_path_is_sibling_of_final() {
        let final_path = PathBuf::from("/cache/oci/ghcr.io/features/node/sha256-abc");
        let staging = FeatureCache::staging_path(&final_path);
        assert_eq!(staging.parent(), final_path.parent());
    }

    // -----------------------------------------------------------------------
    // commit: success and race
    // -----------------------------------------------------------------------

    #[test]
    fn atomic_commit_success() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir
            .path()
            .join("oci")
            .join("ghcr.io")
            .join("features")
            .join("sha256-abc");
        let staging = FeatureCache::staging_path(&final_path);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("marker"), b"ok").unwrap();

        FeatureCache::commit(&staging, &final_path).unwrap();

        assert!(final_path.is_dir());
        assert_eq!(
            std::fs::read_to_string(final_path.join("marker")).unwrap(),
            "ok"
        );
        assert!(!staging.exists());
    }

    #[test]
    fn atomic_commit_race_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("target");
        // Simulate the winner: final_path already exists.
        std::fs::create_dir_all(&final_path).unwrap();
        std::fs::write(final_path.join("winner"), b"first").unwrap();

        // Our staging dir has different content.
        let staging = FeatureCache::staging_path(&final_path);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("loser"), b"second").unwrap();

        // commit should succeed (loser cleans up).
        FeatureCache::commit(&staging, &final_path).unwrap();

        // Winner's content is preserved.
        assert_eq!(
            std::fs::read_to_string(final_path.join("winner")).unwrap(),
            "first"
        );
        // Staging dir cleaned up.
        assert!(!staging.exists());
    }

    // -----------------------------------------------------------------------
    // oci_path security: path traversal and injection
    // -----------------------------------------------------------------------

    /// Assert that `path` is strictly contained under `root` with no traversal
    /// components and the expected `oci/<reg>/<repo>/<dig>` shape.
    ///
    /// `Path::starts_with` is a lexical prefix check and returns `true` for
    /// `root/oci/../../escape`, so it cannot prove containment on its own.
    /// This helper additionally:
    ///   - rejects any `..` or `.` components in the suffix past root
    ///   - rejects any `RootDir` component in the suffix (absolute injection)
    ///   - asserts the suffix is exactly 4 segments: `oci`, reg-hash,
    ///     repo-hash, digest-hash
    fn assert_oci_path_shape(root: &Path, path: &Path) {
        use std::path::Component;

        // Strip the root prefix — this also verifies the path shares the root.
        let suffix = path
            .strip_prefix(root)
            .unwrap_or_else(|_| panic!("path {path:?} does not start under root {root:?}"));

        let components: Vec<_> = suffix.components().collect();

        // Exactly 4 segments past root: "oci", reg-hash, repo-hash, dig-hash.
        assert_eq!(
            components.len(),
            4,
            "expected exactly 4 path components past root (oci/<reg>/<repo>/<dig>), \
             got {}: {path:?}",
            components.len(),
        );

        for c in &components {
            match c {
                Component::Normal(_) => {}
                Component::ParentDir => {
                    panic!("path contains `..` traversal component: {path:?}");
                }
                Component::CurDir => {
                    panic!("path contains `.` component: {path:?}");
                }
                Component::RootDir | Component::Prefix(_) => {
                    panic!("path contains absolute injection component: {path:?}");
                }
            }
        }

        assert_eq!(
            components[0],
            Component::Normal(std::ffi::OsStr::new("oci")),
            "first segment must be \"oci\": {path:?}",
        );

        // Each of the three hash segments must be a 64-character hex string.
        for (i, c) in components[1..].iter().enumerate() {
            let Component::Normal(seg) = c else {
                unreachable!();
            };
            let s = seg.to_string_lossy();
            assert_eq!(
                s.len(),
                64,
                "hash segment {i} must be 64 hex chars, got {}: {path:?}",
                s.len(),
            );
            assert!(
                s.chars().all(|ch| ch.is_ascii_hexdigit()),
                "hash segment {i} contains non-hex characters: {path:?}",
            );
        }
    }

    #[test]
    fn oci_path_dotdot_registry_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("../../escape", "repo", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn oci_path_dotdot_repository_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("ghcr.io", "../../escape", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn oci_path_dotdot_digest_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("ghcr.io", "repo", "../../escape");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn oci_path_absolute_registry_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("/etc/passwd", "repo", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn oci_path_embedded_separator_in_repository() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        // OCI repos legitimately contain slashes (e.g. org/image/name).
        // After hashing the whole component, the path stays flat.
        let path = cache.oci_path("ghcr.io", "devcontainers/features/node", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    /// Regression: empty ref components must still produce the 4-segment
    /// `oci/<reg>/<repo>/<dig>` shape and must not collapse path depth.
    #[test]
    fn oci_path_empty_components_preserve_depth() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("", "", "");
        assert_oci_path_shape(dir.path(), &path);
        // Each empty string hashes to a distinct 64-char hex value, so all
        // three hashes are equal (SHA-256("") is the same each time) but still
        // present — no segment collapses.
        let depth = path.components().count();
        let root_depth = dir.path().components().count();
        assert_eq!(depth - root_depth, 4, "expected oci/<reg>/<repo>/<dig>");
    }

    #[test]
    fn oci_path_distinct_repos_produce_distinct_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let a = cache.oci_path("ghcr.io", "devcontainers/features/node", "sha256:abc");
        let b = cache.oci_path("ghcr.io", "devcontainers/features/python", "sha256:abc");
        assert_ne!(a, b);
    }

    #[test]
    fn oci_path_same_inputs_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let a = cache.oci_path("ghcr.io", "devcontainers/features/node", "sha256:abc");
        let b = cache.oci_path("ghcr.io", "devcontainers/features/node", "sha256:abc");
        assert_eq!(a, b);
    }

    // -----------------------------------------------------------------------
    // build_context_path
    // -----------------------------------------------------------------------

    #[test]
    fn build_context_path_under_builds() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path().join("features"));
        let path = cache.build_context_path("abc123");
        assert!(path.ends_with("builds/abc123"));
        // Should be a sibling of the features root, not nested inside it.
        assert!(!path.starts_with(dir.path().join("features")));
    }
}
