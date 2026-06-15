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
///
/// When `dry_run` is true, no destructive operations are performed — the
/// returned `PruneResult` describes what *would* be pruned.
pub async fn execute_prune(
    repo_root: &Path,
    client: &dyn ContainerBackend,
    candidates: &[PruneCandidate],
    progress: &ProgressSender,
    hooks: &dyn PruneHooks,
    dry_run: bool,
) -> PruneResult {
    let mut pruned = Vec::new();
    let mut errors = Vec::new();

    for candidate in candidates {
        if dry_run {
            pruned.push(PrunedEntry {
                branch: candidate.branch.clone(),
                had_container: candidate.container.is_some(),
            });
            continue;
        }

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
                if let Err(e) = client.remove_container(&container.id, false).await {
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
                    match client
                        .remove_workspace_network(&candidate.worktree_path)
                        .await
                    {
                        Ok(outcome) => debug!(
                            branch = %candidate.branch,
                            ?outcome,
                            "workspace network cleanup"
                        ),
                        Err(e) => debug!(
                            branch = %candidate.branch,
                            "workspace network cleanup failed (non-fatal): {e}"
                        ),
                    }
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

    if !dry_run {
        // 4. Clean up stale git worktree records
        let _ = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_root)
            .output();

        // 5. Stop daemon if no containers remain
        hooks.cleanup_daemon();
    }

    PruneResult { pruned, errors }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use cella_backend::{BackendError, BackendKind, BoxFuture};

    struct SpyHooks {
        deregister_called: AtomicBool,
        cleanup_called: AtomicBool,
    }

    impl SpyHooks {
        fn new() -> Self {
            Self {
                deregister_called: AtomicBool::new(false),
                cleanup_called: AtomicBool::new(false),
            }
        }
    }

    impl PruneHooks for SpyHooks {
        fn deregister_container(
            &self,
            _container: &ContainerInfo,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.deregister_called.store(true, Ordering::Relaxed);
            Box::pin(async {})
        }

        fn compose_down(
            &self,
            _project_name: &str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn cleanup_daemon(&self) {
            self.cleanup_called.store(true, Ordering::Relaxed);
        }
    }

    struct PanicBackend;

    macro_rules! panic_method {
        ($name:ident, $($arg:ident: $ty:ty),* => $ret:ty) => {
            fn $name<'a>(&'a self, $(_: $ty),*) -> BoxFuture<'a, $ret> {
                panic!(concat!(stringify!($name), " must not be called in dry-run"));
            }
        };
    }

    impl ContainerBackend for PanicBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }

        fn capabilities(&self) -> cella_backend::BackendCapabilities {
            cella_backend::BackendCapabilities {
                compose: false,
                managed_agent: false,
            }
        }

        panic_method!(find_container, w: &'a Path => Result<Option<ContainerInfo>, BackendError>);
        panic_method!(create_container, o: &'a cella_backend::CreateContainerOptions => Result<String, BackendError>);
        panic_method!(start_container, id: &'a str => Result<(), BackendError>);
        panic_method!(stop_container, id: &'a str => Result<(), BackendError>);
        fn remove_container<'a>(
            &'a self,
            _id: &'a str,
            _remove_volumes: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("remove_container must not be called in dry-run");
        }
        panic_method!(inspect_container, id: &'a str => Result<ContainerInfo, BackendError>);
        fn list_cella_containers(
            &self,
            _running_only: bool,
        ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
            panic!("list_cella_containers must not be called in dry-run");
        }
        fn find_compose_service<'a>(
            &'a self,
            _project: &'a str,
            _service: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            panic!("find_compose_service must not be called in dry-run");
        }
        fn find_container_by_label<'a>(
            &'a self,
            _label: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            panic!("find_container_by_label must not be called in dry-run");
        }
        fn container_logs<'a>(
            &'a self,
            _id: &'a str,
            _tail: u32,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            panic!("container_logs must not be called in dry-run");
        }
        fn exec_command<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a cella_backend::ExecOptions,
        ) -> BoxFuture<'a, Result<cella_backend::ExecResult, BackendError>> {
            panic!("exec_command must not be called in dry-run");
        }
        fn exec_stream<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a cella_backend::ExecOptions,
            _stdout: Box<dyn std::io::Write + Send + 'a>,
            _stderr: Box<dyn std::io::Write + Send + 'a>,
        ) -> BoxFuture<'a, Result<cella_backend::ExecResult, BackendError>> {
            panic!("exec_stream must not be called in dry-run");
        }
        fn exec_interactive<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a cella_backend::InteractiveExecOptions,
        ) -> BoxFuture<'a, Result<i64, BackendError>> {
            panic!("exec_interactive must not be called in dry-run");
        }
        fn exec_detached<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a cella_backend::ExecOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            panic!("exec_detached must not be called in dry-run");
        }
        panic_method!(pull_image, image: &'a str => Result<(), BackendError>);
        panic_method!(build_image, opts: &'a cella_backend::BuildOptions => Result<String, BackendError>);
        panic_method!(image_exists, image: &'a str => Result<bool, BackendError>);
        panic_method!(tag_image, source: &'a str, target: &'a str => Result<(), BackendError>);
        panic_method!(inspect_image_details, image: &'a str => Result<cella_backend::ImageDetails, BackendError>);
        fn upload_files<'a>(
            &'a self,
            _container_id: &'a str,
            _files: &'a [cella_backend::FileToUpload],
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("upload_files must not be called in dry-run");
        }
        fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            panic!("ping must not be called in dry-run");
        }
        fn host_gateway(&self) -> &'static str {
            "host.docker.internal"
        }
        fn detect_platform(&self) -> BoxFuture<'_, Result<cella_backend::Platform, BackendError>> {
            panic!("detect_platform must not be called in dry-run");
        }
        fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
            panic!("detect_container_arch must not be called in dry-run");
        }
        fn inspect_image_env<'a>(
            &'a self,
            _image: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
            panic!("inspect_image_env must not be called in dry-run");
        }
        fn inspect_image_user<'a>(
            &'a self,
            _image: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            panic!("inspect_image_user must not be called in dry-run");
        }
        fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            panic!("ensure_network must not be called in dry-run");
        }
        fn ensure_container_network<'a>(
            &'a self,
            _container_id: &'a str,
            _repo_path: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("ensure_container_network must not be called in dry-run");
        }
        fn get_container_ip<'a>(
            &'a self,
            _container_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
            panic!("get_container_ip must not be called in dry-run");
        }
        fn ensure_agent_provisioned<'a>(
            &'a self,
            _version: &'a str,
            _arch: &'a str,
            _skip_checksum: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("ensure_agent_provisioned must not be called in dry-run");
        }
        fn write_agent_addr<'a>(
            &'a self,
            _container_id: &'a str,
            _addr: &'a str,
            _token: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("write_agent_addr must not be called in dry-run");
        }
        fn agent_volume_mount(&self) -> (String, String, bool) {
            panic!("agent_volume_mount must not be called in dry-run");
        }
        fn prune_old_agent_versions<'a>(
            &'a self,
            _current_version: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("prune_old_agent_versions must not be called in dry-run");
        }
    }

    fn dummy_container(name: &str) -> ContainerInfo {
        ContainerInfo {
            id: format!("{name}-id"),
            name: name.to_string(),
            image: Some("test:latest".to_string()),
            state: cella_backend::ContainerState::Running,
            exit_code: None,
            labels: HashMap::new(),
            config_hash: None,
            ports: vec![],
            created_at: None,
            started_at: None,
            container_user: None,
            mounts: vec![],
            backend: BackendKind::Docker,
        }
    }

    #[tokio::test]
    async fn dry_run_returns_candidates_without_side_effects() {
        let hooks = SpyHooks::new();
        let backend = PanicBackend;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let progress = ProgressSender::new(tx, false);

        let candidates = vec![
            PruneCandidate {
                branch: "feat-a".to_string(),
                worktree_path: PathBuf::from("/tmp/wt-a"),
                container: Some(dummy_container("cella-feat-a")),
                reason: PruneReason::Merged,
            },
            PruneCandidate {
                branch: "feat-b".to_string(),
                worktree_path: PathBuf::from("/tmp/wt-b"),
                container: None,
                reason: PruneReason::Gone,
            },
        ];

        let result = execute_prune(
            Path::new("/tmp"),
            &backend,
            &candidates,
            &progress,
            &hooks,
            true,
        )
        .await;

        assert_eq!(result.pruned.len(), 2);
        assert_eq!(result.pruned[0].branch, "feat-a");
        assert!(result.pruned[0].had_container);
        assert_eq!(result.pruned[1].branch, "feat-b");
        assert!(!result.pruned[1].had_container);
        assert!(result.errors.is_empty());
        assert!(
            !hooks.deregister_called.load(Ordering::Relaxed),
            "dry-run must not deregister containers"
        );
        assert!(
            !hooks.cleanup_called.load(Ordering::Relaxed),
            "dry-run must not call cleanup_daemon"
        );
    }
}
