//! Advisory file locking for concurrent worktree operations.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use tracing::debug;

use crate::CellaGitError;

/// An advisory file lock scoped to a branch within a repository.
///
/// Acquiring this lock ensures that only one process at a time can perform
/// worktree operations for a given branch. The lock is automatically released
/// when dropped.
#[derive(Debug)]
pub struct BranchLock {
    _file: File,
    path: PathBuf,
}

impl BranchLock {
    /// Acquire an exclusive advisory lock for a branch.
    ///
    /// Creates `{repo_root}/.git/cella-{sanitized_branch}.lock` and acquires
    /// an exclusive `flock` on it. Blocks until the lock is available.
    ///
    /// # Errors
    ///
    /// Returns `NotARepository` if `.git` doesn't exist, or `Io` on lock failure.
    pub fn acquire(repo_root: &Path, branch: &str) -> Result<Self, CellaGitError> {
        let lock_dir = repo_root.join(".git");
        if !lock_dir.is_dir() {
            return Err(CellaGitError::NotARepository {
                path: repo_root.to_path_buf(),
            });
        }

        let sanitized = crate::sanitize::branch_to_dir_name(branch);
        let lock_path = lock_dir.join(format!("cella-{sanitized}.lock"));

        let file = File::create(&lock_path).map_err(CellaGitError::Io)?;
        file.lock_exclusive().map_err(|e| {
            debug!("failed to acquire lock at {}: {e}", lock_path.display());
            CellaGitError::Io(e)
        })?;

        debug!("acquired branch lock: {}", lock_path.display());
        Ok(Self {
            _file: file,
            path: lock_path,
        })
    }

    /// Try to acquire the lock without blocking.
    ///
    /// Returns `Ok(None)` if the lock is held by another process.
    ///
    /// # Errors
    ///
    /// Returns `NotARepository` if `.git` doesn't exist, or `Io` on lock failure.
    pub fn try_acquire(repo_root: &Path, branch: &str) -> Result<Option<Self>, CellaGitError> {
        let lock_dir = repo_root.join(".git");
        if !lock_dir.is_dir() {
            return Err(CellaGitError::NotARepository {
                path: repo_root.to_path_buf(),
            });
        }

        let sanitized = crate::sanitize::branch_to_dir_name(branch);
        let lock_path = lock_dir.join(format!("cella-{sanitized}.lock"));

        let file = File::create(&lock_path).map_err(CellaGitError::Io)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                debug!("acquired branch lock: {}", lock_path.display());
                Ok(Some(Self {
                    _file: file,
                    path: lock_path,
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(CellaGitError::Io(e)),
        }
    }

    /// Best-effort cleanup of the lock file on drop.
    fn cleanup(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for BranchLock {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::init_repo;

    #[test]
    fn acquire_and_drop_releases_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let lock = BranchLock::acquire(tmp.path(), "test-branch").unwrap();
        let lock_path = lock.path.clone();
        assert!(lock_path.exists());
        drop(lock);
    }

    #[test]
    fn try_acquire_succeeds_when_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let lock = BranchLock::try_acquire(tmp.path(), "test-branch").unwrap();
        assert!(lock.is_some());
    }

    #[test]
    fn try_acquire_returns_none_when_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let _held = BranchLock::acquire(tmp.path(), "feature/x").unwrap();
        let attempt = BranchLock::try_acquire(tmp.path(), "feature/x").unwrap();
        assert!(attempt.is_none());
    }

    #[test]
    fn different_branches_do_not_block() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let _lock_a = BranchLock::acquire(tmp.path(), "branch-a").unwrap();
        let lock_b = BranchLock::try_acquire(tmp.path(), "branch-b").unwrap();
        assert!(lock_b.is_some());
    }

    #[test]
    fn not_a_repo_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = BranchLock::acquire(tmp.path(), "test");
        assert!(matches!(
            result.unwrap_err(),
            CellaGitError::NotARepository { .. }
        ));
    }
}
