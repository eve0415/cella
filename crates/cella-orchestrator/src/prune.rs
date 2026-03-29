//! Worktree pruning: detect and remove merged worktrees with their containers.

use std::path::Path;

use tracing::info;

use cella_docker::DockerClient;

use crate::error::OrchestratorError;
use crate::progress::ProgressSender;
use crate::result::{PruneResult, PrunedEntry};

/// A worktree that is a candidate for pruning.
pub struct PruneCandidate {
    /// Branch name.
    pub branch: String,
    /// Worktree directory path on the host.
    pub worktree_path: std::path::PathBuf,
    /// Associated container name, if any.
    pub container_name: Option<String>,
    /// Associated container ID, if any.
    pub container_id: Option<String>,
    /// Container state string (e.g. "running", "stopped").
    pub container_state: Option<String>,
}

/// Find worktrees whose branches are fully merged into the default branch.
///
/// # Errors
///
/// Returns an error if git operations fail.
pub async fn prune_candidates(
    repo_root: &Path,
    client: &DockerClient,
) -> Result<Vec<PruneCandidate>, OrchestratorError> {
    let worktrees = cella_git::list(repo_root).map_err(|e| OrchestratorError::Git {
        message: format!("failed to list worktrees: {e}"),
    })?;

    let linked: Vec<_> = worktrees.into_iter().filter(|wt| !wt.is_main).collect();
    if linked.is_empty() {
        return Ok(vec![]);
    }

    let default_branch =
        cella_git::default_branch(repo_root).map_err(|e| OrchestratorError::Git {
            message: format!("failed to detect default branch: {e}"),
        })?;

    let merged =
        cella_git::merged_branches(repo_root, &default_branch).map_err(|e| OrchestratorError::Git {
            message: format!("failed to list merged branches: {e}"),
        })?;

    let mut candidates = Vec::new();
    for wt in &linked {
        let Some(branch) = &wt.branch else { continue };
        if !merged.contains(branch) {
            continue;
        }

        let container = client.find_container(&wt.path).await.ok().flatten();
        candidates.push(PruneCandidate {
            branch: branch.clone(),
            worktree_path: wt.path.clone(),
            container_name: container.as_ref().map(|c| c.name.clone()),
            container_id: container.as_ref().map(|c| c.id.clone()),
            container_state: container
                .as_ref()
                .map(|c| format!("{:?}", c.state).to_lowercase()),
        });
    }

    Ok(candidates)
}

/// Execute pruning: remove containers and worktrees for the given candidates.
///
/// # Errors
///
/// Returns a result with pruned entries and any errors encountered.
pub async fn execute_prune(
    repo_root: &Path,
    client: &DockerClient,
    candidates: &[PruneCandidate],
    progress: &ProgressSender,
) -> PruneResult {
    let mut pruned = Vec::new();
    let mut errors = Vec::new();

    for candidate in candidates {
        // Stop and remove container.
        if let Some(ref container_id) = candidate.container_id {
            let _ = client.stop_container(container_id).await;
            let _ = client.remove_container(container_id, false).await;
            info!(
                branch = %candidate.branch,
                container = candidate.container_name.as_deref().unwrap_or("-"),
                "removed container"
            );
        }

        // Remove worktree.
        match cella_git::remove(repo_root, &candidate.worktree_path) {
            Ok(()) => {
                let had_container = candidate.container_id.is_some();
                progress.println(&format!(
                    "  Pruned: {} ({})",
                    candidate.branch,
                    if had_container {
                        "container removed"
                    } else {
                        "no container"
                    }
                ));
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

    // Clean up stale git worktree records.
    let _ = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_root)
        .output();

    PruneResult { pruned, errors }
}
