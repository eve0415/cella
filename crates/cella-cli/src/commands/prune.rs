use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use clap::Args;
use tracing::{debug, info, warn};

use cella_compose::discovery;
use cella_docker::ContainerInfo;

use super::up::OutputFormat;

/// Remove stale worktrees and their associated containers.
///
/// Identifies worktrees whose branches have been fully merged into the
/// default branch or whose remote tracking ref has been deleted (e.g. after
/// a squash-merge on GitHub), then removes the worktree, container, and
/// local branch.
#[derive(Args)]
pub struct PruneArgs {
    /// Remove without confirmation.
    #[arg(long)]
    force: bool,

    /// Show what would be removed without doing it.
    #[arg(long)]
    dry_run: bool,

    /// Include unmerged worktrees (not just merged ones).
    #[arg(long)]
    all: bool,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Output format (text or json).
    #[arg(long, default_value = "text")]
    output: OutputFormat,
}

/// Why a worktree was selected for pruning.
#[derive(Debug, Clone, Copy)]
enum PruneReason {
    /// Branch is fully merged into the default branch.
    Merged,
    /// Remote tracking ref was deleted (squash-merge or manual deletion).
    Gone,
    /// Included via `--all` but not merged or gone.
    Unmerged,
}

impl PruneReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Gone => "gone",
            Self::Unmerged => "unmerged",
        }
    }
}

/// A worktree that is a candidate for pruning.
struct PruneCandidate {
    branch: String,
    worktree_path: PathBuf,
    container: Option<ContainerInfo>,
    reason: PruneReason,
}

impl PruneArgs {
    const fn is_json(&self) -> bool {
        matches!(self.output, OutputFormat::Json)
    }

    pub async fn execute(
        self,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Discover git repo
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd)?;
        let repo_root = &repo_info.root;

        // 2. List all worktrees
        let worktrees = cella_git::list(repo_root)?;
        let linked_worktrees: Vec<_> = worktrees.into_iter().filter(|wt| !wt.is_main).collect();

        if linked_worktrees.is_empty() {
            if self.is_json() {
                print_json_result(&[], &[]);
            } else {
                eprintln!("No linked worktrees found.");
            }
            return Ok(());
        }

        // 3. Fetch, detect, and build candidates
        let _ = backend; // TODO: use resolve_backend once prune internals are migrated
        let client = super::connect_docker(self.docker_host.as_deref())?;
        let candidates = self
            .build_candidates(repo_root, &linked_worktrees, &client)
            .await?;

        if candidates.is_empty() {
            if self.is_json() {
                print_json_result(&[], &[]);
            } else if self.all {
                eprintln!("Nothing to prune. No linked worktrees found.");
            } else {
                eprintln!("Nothing to prune. No merged or gone worktrees found.");
            }
            return Ok(());
        }

        // 4. Display, confirm, and execute
        self.confirm_and_prune(candidates, &client, repo_root).await
    }

    async fn build_candidates(
        &self,
        repo_root: &std::path::Path,
        linked_worktrees: &[cella_git::WorktreeInfo],
        client: &cella_docker::DockerClient,
    ) -> Result<Vec<PruneCandidate>, Box<dyn std::error::Error>> {
        if !self.is_json() {
            eprintln!("Fetching remote refs...");
        }
        if let Err(e) = cella_git::fetch_prune(repo_root) {
            if !self.is_json() {
                eprintln!("Warning: git fetch --prune failed: {e}");
            }
            warn!("git fetch --prune failed: {e}");
        }

        let merged = if self.all {
            Vec::new()
        } else {
            let default_branch = cella_git::default_branch(repo_root)?;
            debug!("default branch: {default_branch}");
            let merged = cella_git::merged_branches(repo_root, &default_branch)?;
            debug!("merged branches: {merged:?}");
            merged
        };

        let mut candidates = Vec::new();
        for wt in linked_worktrees {
            let Some(branch) = &wt.branch else {
                continue; // Skip detached HEAD worktrees
            };

            let reason = classify_branch(repo_root, branch, &merged, self.all);
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

    async fn confirm_and_prune(
        &self,
        candidates: Vec<PruneCandidate>,
        client: &cella_docker::DockerClient,
        repo_root: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.is_json() {
            print_candidates(&candidates);
        }

        if self.dry_run {
            if self.is_json() {
                let branch_names: Vec<&str> =
                    candidates.iter().map(|c| c.branch.as_str()).collect();
                print_json_result(&branch_names, &[]);
            } else {
                eprintln!("\nDry run — no changes made.");
            }
            return Ok(());
        }

        if !self.force && !self.is_json() {
            let unmerged = if self.all {
                " (including unmerged)"
            } else {
                ""
            };
            let plural = if candidates.len() == 1 { "" } else { "s" };
            eprint!(
                "\nRemove {} worktree{plural}{unmerged}? [y/N] ",
                candidates.len()
            );
            io::stderr().flush()?;

            let stdin = io::stdin();
            let mut line = String::new();
            stdin.lock().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                eprintln!("Aborted.");
                return Ok(());
            }
        }

        let (pruned_branches, errors) =
            execute_prune(&candidates, client, repo_root, self.is_json()).await;

        if self.is_json() {
            let names: Vec<&str> = pruned_branches.iter().map(String::as_str).collect();
            print_json_result(&names, &errors);
        } else {
            let count = pruned_branches.len();
            eprintln!(
                "\nPruned {count} worktree{}",
                if count == 1 { "" } else { "s" }
            );
        }
        Ok(())
    }
}

/// Classify a branch for pruning. Returns `None` if it should be skipped.
fn classify_branch(
    repo_root: &std::path::Path,
    branch: &str,
    merged: &[String],
    include_all: bool,
) -> Option<PruneReason> {
    if include_all {
        Some(if merged.contains(&branch.to_string()) {
            PruneReason::Merged
        } else if cella_git::is_tracking_gone(repo_root, branch).unwrap_or(false) {
            PruneReason::Gone
        } else {
            PruneReason::Unmerged
        })
    } else if merged.contains(&branch.to_string()) {
        Some(PruneReason::Merged)
    } else if cella_git::is_tracking_gone(repo_root, branch).unwrap_or(false) {
        Some(PruneReason::Gone)
    } else {
        None
    }
}

async fn execute_prune(
    candidates: &[PruneCandidate],
    client: &cella_docker::DockerClient,
    repo_root: &std::path::Path,
    json_mode: bool,
) -> (Vec<String>, Vec<String>) {
    let mut pruned_branches = Vec::new();
    let mut errors = Vec::new();

    for candidate in candidates {
        // Stop and remove container
        if let Some(ref container) = candidate.container {
            super::down::deregister_container(container).await;

            if discovery::is_compose_container(&container.labels)
                && let Some(project_name) =
                    discovery::compose_project_from_labels(&container.labels)
            {
                let compose_cmd = cella_compose::ComposeCommand::from_project_name(project_name);
                if let Err(e) = compose_cmd.down().await {
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

        // Remove worktree
        match cella_git::remove(repo_root, &candidate.worktree_path) {
            Ok(()) => {
                if !json_mode {
                    eprintln!(
                        "  Pruned: {} ({})",
                        candidate.branch,
                        if candidate.container.is_some() {
                            "container removed"
                        } else {
                            "no container"
                        }
                    );
                }
                pruned_branches.push(candidate.branch.clone());

                // Delete the local branch
                if let Err(e) = cella_git::delete_branch(repo_root, &candidate.branch) {
                    debug!(
                        branch = %candidate.branch,
                        "failed to delete branch (may already be gone): {e}"
                    );
                }
            }
            Err(e) => {
                let msg = format!("failed to remove worktree for {}: {e}", candidate.branch);
                if !json_mode {
                    eprintln!("  {msg}");
                }
                errors.push(msg);
            }
        }
    }

    // Clean up stale git worktree records
    let _ = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_root)
        .output();

    super::down::cleanup_daemon();

    (pruned_branches, errors)
}

fn print_json_result(pruned: &[&str], errors: &[String]) {
    let result = serde_json::json!({
        "pruned": pruned,
        "errors": errors,
    });
    println!("{result}");
}

fn format_candidates(candidates: &[PruneCandidate]) -> String {
    use crate::table::{Column, Table};

    let mut table = Table::new(vec![
        Column::fixed("BRANCH"),
        Column::shrinkable("WORKTREE"),
        Column::fixed("CONTAINER"),
        Column::fixed("STATE"),
        Column::fixed("REASON"),
    ]);

    for c in candidates {
        table.add_row(vec![
            c.branch.clone(),
            c.worktree_path.display().to_string(),
            c.container
                .as_ref()
                .map_or_else(|| "-".to_string(), |ci| ci.name.clone()),
            c.container.as_ref().map_or_else(
                || "-".to_string(),
                |ci| format!("{:?}", ci.state).to_lowercase(),
            ),
            c.reason.as_str().to_string(),
        ]);
    }

    table.render()
}

fn print_candidates(candidates: &[PruneCandidate]) {
    eprint!("{}", format_candidates(candidates));
}

#[cfg(test)]
mod tests {
    use super::*;

    use cella_docker::ContainerState;

    fn make_container(name: &str, id: &str, state: ContainerState) -> ContainerInfo {
        ContainerInfo {
            id: id.to_string(),
            name: name.to_string(),
            state,
            exit_code: None,
            labels: std::collections::HashMap::new(),
            config_hash: None,
            ports: vec![],
            created_at: None,
            container_user: None,
            image: None,
            mounts: vec![],
            backend: cella_backend::BackendKind::Docker,
        }
    }

    #[test]
    fn format_candidates_with_containers() {
        let candidates = vec![
            PruneCandidate {
                branch: "fix/typo".to_string(),
                worktree_path: PathBuf::from("/workspaces/cella-worktrees/fix-typo"),
                container: Some(make_container(
                    "cella-fix-typo-abc12",
                    "abc12345",
                    ContainerState::Stopped,
                )),
                reason: PruneReason::Merged,
            },
            PruneCandidate {
                branch: "feat/old".to_string(),
                worktree_path: PathBuf::from("/workspaces/cella-worktrees/feat-old"),
                container: Some(make_container(
                    "cella-feat-old-def34",
                    "def34567",
                    ContainerState::Running,
                )),
                reason: PruneReason::Gone,
            },
        ];

        let output = format_candidates(&candidates);
        insta::assert_snapshot!(output, @"
        BRANCH    WORKTREE                              CONTAINER             STATE    REASON
        fix/typo  /workspaces/cella-worktrees/fix-typo  cella-fix-typo-abc12  stopped  merged
        feat/old  /workspaces/cella-worktrees/feat-old  cella-feat-old-def34  running  gone
        ");
    }

    #[test]
    fn format_candidates_without_containers() {
        let candidates = vec![PruneCandidate {
            branch: "orphan".to_string(),
            worktree_path: PathBuf::from("/worktrees/orphan"),
            container: None,
            reason: PruneReason::Gone,
        }];

        let output = format_candidates(&candidates);
        insta::assert_snapshot!(output, @"
        BRANCH  WORKTREE           CONTAINER  STATE  REASON
        orphan  /worktrees/orphan  -          -      gone
        ");
    }

    #[test]
    fn format_candidates_empty() {
        let candidates: Vec<PruneCandidate> = vec![];
        let output = format_candidates(&candidates);
        // Only the header line
        assert_eq!(output.lines().count(), 1);
        assert!(output.starts_with("BRANCH"));
    }

    #[test]
    fn format_candidates_long_paths() {
        let candidates = vec![PruneCandidate {
            branch: "feat/a-very-long-branch-name".to_string(),
            worktree_path: PathBuf::from("/very/long/worktree/path/feat-a-very-long-branch-name"),
            container: Some(make_container(
                "cella-feat-long-branch-name-xyz99",
                "xyz99",
                ContainerState::Stopped,
            )),
            reason: PruneReason::Unmerged,
        }];

        let output = format_candidates(&candidates);
        assert!(output.contains("feat/a-very-long-branch-name"));
        assert!(output.contains("stopped"));
        assert!(output.contains("unmerged"));
    }

    #[test]
    fn print_json_result_empty() {
        // Just verify it doesn't panic with empty inputs
        print_json_result(&[], &[]);
    }

    #[test]
    fn print_json_result_with_errors() {
        // Verify it doesn't panic with error messages
        print_json_result(&["main"], &["failed to remove worktree".to_string()]);
    }

    #[test]
    fn prune_args_is_json_text() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "prune"]).unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert!(!args.is_json());
        }
    }

    #[test]
    fn prune_args_is_json_json() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "prune", "--output", "json"]).unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert!(args.is_json());
        }
    }

    #[test]
    fn prune_reason_as_str() {
        assert_eq!(PruneReason::Merged.as_str(), "merged");
        assert_eq!(PruneReason::Gone.as_str(), "gone");
        assert_eq!(PruneReason::Unmerged.as_str(), "unmerged");
    }
}
