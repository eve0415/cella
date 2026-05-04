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

/// Result of worktree creation, indicating whether a new worktree was created
/// or an existing one was reused.
#[derive(Debug, Clone)]
pub struct WorktreeResult {
    /// Path to the worktree directory.
    pub path: PathBuf,
    /// Whether the worktree was freshly created (`true`) or already existed (`false`).
    pub created: bool,
}

/// Create a git worktree for a branch (idempotent).
///
/// If the worktree already exists for the given branch, returns its path with
/// `created: false`. Otherwise creates a new worktree and returns `created: true`.
///
/// Does NOT create a container — the caller is responsible for running the
/// container-up pipeline on the returned path.
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
) -> Result<WorktreeResult, OrchestratorError> {
    let wt_path = cella_git::worktree_path(repo_root, branch, worktree_root);
    let already_existed = wt_path.exists();

    let step = if already_existed {
        progress.step(&format!("Reusing worktree for branch '{branch}'..."))
    } else {
        progress.step(&format!("Creating worktree for branch '{branch}'..."))
    };

    let branch_state =
        cella_git::resolve_branch(repo_root, branch).map_err(|e| OrchestratorError::Git {
            message: format!("failed to resolve branch '{branch}': {e}"),
        })?;

    info!(
        branch = branch,
        state = ?branch_state,
        already_existed = already_existed,
        "resolved branch state"
    );

    let wt_info = cella_git::create(repo_root, branch, &branch_state, worktree_root, base)
        .map_err(|e| OrchestratorError::Git {
            message: format!("failed to create worktree for '{branch}': {e}"),
        })?;

    if already_existed {
        step.finish_with(&format!("Worktree reused at {}", wt_info.path.display()));
    } else {
        step.finish_with(&format!("Worktree created at {}", wt_info.path.display()));
    }

    Ok(WorktreeResult {
        path: wt_info.path,
        created: !already_existed,
    })
}

/// Remove a git worktree (rollback helper).
///
/// Used when container creation fails after worktree creation.
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path) {
    if let Err(e) = cella_git::remove(repo_root, worktree_path) {
        tracing::warn!("failed to clean up worktree: {e}");
    }
}
