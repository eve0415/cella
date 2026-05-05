//! Advisory file locking for concurrent worktree operations.

use std::fs::File;
use std::path::Path;

use tracing::debug;

use crate::CellaGitError;

/// An advisory file lock scoped to a branch within a repository.
///
/// Acquiring this lock ensures that only one process at a time can perform
/// worktree operations for a given branch. The lock is automatically released
/// when the file handle is dropped (flock semantics).
///
/// Lock files are intentionally not deleted on drop to avoid a TOCTOU race
/// where a concurrent process could create a new inode at the same path,
/// breaking mutual exclusion. Stale `.lock` files in `.git/` are harmless.
#[derive(Debug)]
pub struct BranchLock {
    _file: File,
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
        if file.try_lock().is_err() {
            tracing::info!(
                "Waiting for lock on branch '{branch}' (another operation in progress)..."
            );
            file.lock().map_err(|e| {
                debug!("failed to acquire lock at {}: {e}", lock_path.display());
                CellaGitError::Io(e)
            })?;
        }

        debug!("acquired branch lock: {}", lock_path.display());
        Ok(Self { _file: file })
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
        drop(lock);

        // Can re-acquire after drop
        let _lock2 = BranchLock::acquire(tmp.path(), "test-branch").unwrap();
    }

    #[test]
    fn different_branches_do_not_block() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let _lock_a = BranchLock::acquire(tmp.path(), "branch-a").unwrap();
        let _lock_b = BranchLock::acquire(tmp.path(), "branch-b").unwrap();
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
