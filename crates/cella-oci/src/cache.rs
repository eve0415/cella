//! Atomic staging/commit helpers for OCI cache directories.

use std::io;
use std::path::{Path, PathBuf};
use std::process;

/// Return a staging path for the given final path.
///
/// The staging name is `{final_name}.partial-{pid}` placed alongside
/// the final path. The caller should write into this directory, then
/// call [`commit_staging`] to atomically swap it into place.
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
/// Uses `fs::rename` which is atomic on POSIX. If the final path
/// already exists (another process won the race), the staging
/// directory is removed and the existing entry is kept.
///
/// # Errors
///
/// Returns an `io::Error` if both the rename and the cleanup of the
/// staging directory fail.
pub fn commit_staging(staging: &Path, final_path: &Path) -> io::Result<()> {
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(staging, final_path) {
        Ok(()) => Ok(()),
        Err(_) if final_path.exists() => {
            let _ = std::fs::remove_dir_all(staging);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_path_includes_pid() {
        let path = staging_path(Path::new("/cache/features/sha256:abc"));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.contains(".partial-"));
        assert!(name.contains(&process::id().to_string()));
    }

    #[test]
    fn staging_path_is_sibling_of_final() {
        let final_path = Path::new("/cache/features/sha256:abc");
        let staging = staging_path(final_path);
        assert_eq!(staging.parent(), final_path.parent());
    }

    #[test]
    fn commit_staging_atomic_rename() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("test.partial-1");
        let final_path = dir.path().join("test");

        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("data.txt"), "hello").unwrap();

        commit_staging(&staging, &final_path).unwrap();

        assert!(!staging.exists());
        assert!(final_path.exists());
        assert_eq!(
            std::fs::read_to_string(final_path.join("data.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn commit_staging_existing_final_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("test.partial-1");
        let final_path = dir.path().join("test");

        std::fs::create_dir(&staging).unwrap();
        std::fs::create_dir(&final_path).unwrap();
        std::fs::write(final_path.join("data.txt"), "existing").unwrap();

        commit_staging(&staging, &final_path).unwrap();

        assert!(!staging.exists());
        assert_eq!(
            std::fs::read_to_string(final_path.join("data.txt")).unwrap(),
            "existing"
        );
    }
}
