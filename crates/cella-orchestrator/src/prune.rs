//! Worktree pruning: detect and remove merged/gone worktrees with containers.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tracing::{debug, info, warn};

use cella_backend::ContainerBackend;
use cella_backend::ContainerInfo;

use crate::error::OrchestratorError;
use crate::progress::ProgressSender;
use crate::result::{PruneResult, PrunedEntry};

// ---------------------------------------------------------------------------
// Prune reason
// ---------------------------------------------------------------------------

/// Why a worktree was selected for pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneReason {
    /// Branch is fully merged into the default branch.
    Merged,
    /// Remote tracking ref was deleted (squash-merge or manual deletion).
    Gone,
    /// Included via `--all` but not merged or gone.
    Unmerged,
}

impl PruneReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Gone => "gone",
            Self::Unmerged => "unmerged",
        }
    }
}

// ---------------------------------------------------------------------------
// Prune candidate
// ---------------------------------------------------------------------------

/// A worktree that is a candidate for pruning.
pub struct PruneCandidate {
    /// Branch name.
    pub branch: String,
    /// Worktree directory path on the host.
    pub worktree_path: PathBuf,
    /// Associated container, if any.
    pub container: Option<ContainerInfo>,
    /// Why this worktree was selected.
    pub reason: PruneReason,
}

// ---------------------------------------------------------------------------
// Hooks for host-side operations the orchestrator cannot own
// ---------------------------------------------------------------------------

/// Callbacks for operations that live outside the orchestrator's dependency
/// graph (daemon IPC, compose CLI, etc.).
///
/// The CLI provides a real implementation; the daemon can provide stubs or
/// its own variants.
pub trait PruneHooks: Send + Sync {
    /// Deregister a container from the daemon's management table.
    fn deregister_container(
        &self,
        container: &ContainerInfo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Tear down a compose project (equivalent to `docker compose down`).
    fn compose_down(
        &self,
        project_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    /// Stop the daemon if no containers remain.
    fn cleanup_daemon(&self);
}

// ---------------------------------------------------------------------------
// Build candidates
// ---------------------------------------------------------------------------

/// Run `git fetch --prune` and build the list of prune candidates.
///
/// # Errors
///
/// Returns an error if git operations fail.
pub async fn build_prune_candidates(
    repo_root: &Path,
    client: &dyn ContainerBackend,
    all: bool,
) -> Result<Vec<PruneCandidate>, OrchestratorError> {
    // Fetch remote refs so is_tracking_gone is accurate.
    if let Err(e) = cella_git::fetch_prune(repo_root) {
        warn!("git fetch --prune failed: {e}");
    }

    let worktrees = cella_git::list(repo_root).map_err(|e| OrchestratorError::Git {
        message: format!("failed to list worktrees: {e}"),
    })?;

    let linked: Vec<_> = worktrees.into_iter().filter(|wt| !wt.is_main).collect();
    if linked.is_empty() {
        return Ok(vec![]);
    }

    let merged = if all {
        Vec::new()
    } else {
        let default_branch =
            cella_git::default_branch(repo_root).map_err(|e| OrchestratorError::Git {
                message: format!("failed to detect default branch: {e}"),
            })?;

        cella_git::merged_branches(repo_root, &default_branch).map_err(|e| {
            OrchestratorError::Git {
                message: format!("failed to list merged branches: {e}"),
            }
        })?
    };

    let mut candidates = Vec::new();
    for wt in &linked {
        let Some(branch) = &wt.branch else { continue };

        let reason = classify_branch(repo_root, branch, &merged, all);
        let Some(reason) = reason else { continue };

        let container = client.find_container(&wt.path).await.ok().flatten();
        candidates.push(PruneCandidate {
            branch: branch.clone(),
            worktree_path: wt.path.clone(),
            container,
            reason,
        });
    }

    Ok(candidates)
}

/// Classify a branch for pruning. Returns `None` if it should be skipped.
fn classify_branch(
    repo_root: &Path,
    branch: &str,
    merged: &[String],
    include_all: bool,
) -> Option<PruneReason> {
    let is_merged = merged.contains(&branch.to_string());
    let is_gone = cella_git::is_tracking_gone(repo_root, branch).unwrap_or(false);

    if include_all {
        Some(if is_merged {
            PruneReason::Merged
        } else if is_gone {
            PruneReason::Gone
        } else {
            PruneReason::Unmerged
        })
    } else if is_merged {
        Some(PruneReason::Merged)
    } else if is_gone {
        Some(PruneReason::Gone)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Execute prune
// ---------------------------------------------------------------------------

/// Execute pruning: remove containers, worktrees, and branches.
///
/// Compose containers are torn down via `hooks.compose_down()`.
/// Non-compose containers are stopped and removed via the backend.
/// After all candidates are processed, `hooks.cleanup_daemon()` is called.
pub async fn execute_prune(
    repo_root: &Path,
    client: &dyn ContainerBackend,
    candidates: &[PruneCandidate],
    progress: &ProgressSender,
    hooks: &dyn PruneHooks,
) -> PruneResult {
    let mut pruned = Vec::new();
    let mut errors = Vec::new();

    for candidate in candidates {
        // 1. Tear down container
        if let Some(ref container) = candidate.container {
            hooks.deregister_container(container).await;

            if cella_compose::discovery::is_compose_container(&container.labels)
                && let Some(project_name) =
                    cella_compose::discovery::compose_project_from_labels(&container.labels)
            {
                if let Err(e) = hooks.compose_down(project_name).await {
                    errors.push(format!(
                        "failed to stop compose project for {}: {e}",
                        candidate.branch
                    ));
                } else {
                    info!(
                        branch = %candidate.branch,
                        project = project_name,
                        "removed compose project"
                    );
                }
            } else {
                let _ = client.stop_container(&container.id).await;
                if let Err(e) = client.remove_container(&container.id, true).await {
                    errors.push(format!(
                        "failed to remove container {}: {e}",
                        container.name
                    ));
                } else {
                    info!(
                        branch = %candidate.branch,
                        container = %container.name,
                        "removed container"
                    );
                }
            }
        }

        // 2. Remove worktree
        match cella_git::remove(repo_root, &candidate.worktree_path) {
            Ok(()) => {
                let had_container = candidate.container.is_some();
                progress.println(&format!(
                    "  Pruned: {} ({})",
                    candidate.branch,
                    if had_container {
                        "container removed"
                    } else {
                        "no container"
                    }
                ));

                // 3. Delete local branch
                if let Err(e) = cella_git::delete_branch(repo_root, &candidate.branch) {
                    debug!(
                        branch = %candidate.branch,
                        "failed to delete branch (may already be gone): {e}"
                    );
                }

                pruned.push(PrunedEntry {
                    branch: candidate.branch.clone(),
                    had_container,
                });
            }
            Err(e) => {
                let msg = format!("failed to remove worktree for {}: {e}", candidate.branch);
                progress.error(&msg);
                errors.push(msg);
            }
        }
    }

    // 4. Clean up stale git worktree records
    let _ = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_root)
        .output();

    // 5. Stop daemon if no containers remain
    hooks.cleanup_daemon();

    PruneResult { pruned, errors }
}
