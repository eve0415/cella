//! Worktree-backed branch creation.
//!
//! Creates a git worktree for a branch and delegates container creation to the
//! caller-provided up pipeline. This separation allows the daemon to call the
//! worktree logic directly while the container creation path remains in the CLI
//! until the full `UpContext` extraction is complete.

use std::path::{Path, PathBuf};

use tracing::info;

use crate::error::OrchestratorError;
use crate::progress::ProgressSender;

/// Create a git worktree for a branch.
///
/// Returns the worktree path. Does NOT create a container — the caller
/// is responsible for running the container-up pipeline on the returned path.
///
/// # Errors
///
/// Returns an error if git operations fail (branch resolution, worktree creation).
pub fn create_worktree(
    repo_root: &Path,
    branch: &str,
    base: Option<&str>,
    worktree_root: Option<&Path>,
    progress: &ProgressSender,
) -> Result<PathBuf, OrchestratorError> {
    let step = progress.step(&format!("Creating worktree for branch '{branch}'..."));

    let branch_state =
        cella_git::resolve_branch(repo_root, branch).map_err(|e| OrchestratorError::Git {
            message: format!("failed to resolve branch '{branch}': {e}"),
        })?;

    info!(
        branch = branch,
        state = ?branch_state,
        "resolved branch state"
    );

    let wt_info = cella_git::create(repo_root, branch, &branch_state, worktree_root, base)
        .map_err(|e| OrchestratorError::Git {
            message: format!("failed to create worktree for '{branch}': {e}"),
        })?;

    step.finish_with(&format!(
        "Worktree created at {}",
        wt_info.path.display()
    ));

    Ok(wt_info.path)
}

/// Remove a git worktree (rollback helper).
///
/// Used when container creation fails after worktree creation.
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path) {
    if let Err(e) = cella_git::remove(repo_root, worktree_path) {
        tracing::warn!("failed to clean up worktree: {e}");
    }
}
