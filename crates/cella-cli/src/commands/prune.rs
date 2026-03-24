use std::fmt::Write as _;
use std::io::{self, BufRead, Write};

use clap::Args;
use tracing::{debug, info};

/// Remove stale worktrees and their associated containers.
///
/// Identifies worktrees whose branches have been fully merged into the
/// default branch, then removes both the worktree and any associated
/// cella container.
#[derive(Args)]
pub struct PruneArgs {
    /// Remove without confirmation.
    #[arg(long)]
    force: bool,

    /// Show what would be removed without doing it.
    #[arg(long)]
    dry_run: bool,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,
}

/// A worktree that is a candidate for pruning.
struct PruneCandidate {
    branch: String,
    worktree_path: std::path::PathBuf,
    container_name: Option<String>,
    container_id: Option<String>,
    container_state: Option<String>,
}

impl PruneArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Discover git repo
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd)?;
        let repo_root = &repo_info.root;

        // 2. List all worktrees
        let worktrees = cella_git::list(repo_root)?;
        let linked_worktrees: Vec<_> = worktrees.into_iter().filter(|wt| !wt.is_main).collect();

        if linked_worktrees.is_empty() {
            eprintln!("No linked worktrees found.");
            return Ok(());
        }

        // 3. Detect default branch and find merged branches
        let default_branch = cella_git::default_branch(repo_root)?;
        debug!("default branch: {default_branch}");

        let merged = cella_git::merged_branches(repo_root, &default_branch)?;
        debug!("merged branches: {merged:?}");

        // 4. Connect to Docker for container lookup
        let client = super::connect_docker(self.docker_host.as_deref())?;

        // 5. Find prunable worktrees
        let mut candidates = Vec::new();
        for wt in &linked_worktrees {
            let Some(branch) = &wt.branch else {
                continue; // Skip detached HEAD worktrees
            };

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

        if candidates.is_empty() {
            eprintln!("Nothing to prune. No merged worktrees found.");
            return Ok(());
        }

        // 6. Display candidates
        print_candidates(&candidates);

        // 7. Handle dry run
        if self.dry_run {
            eprintln!("\nDry run — no changes made.");
            return Ok(());
        }

        // 8. Confirm unless --force
        if !self.force {
            eprint!(
                "\nRemove {} worktree{} and their containers? [y/N] ",
                candidates.len(),
                if candidates.len() == 1 { "" } else { "s" }
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

        // 9. Prune each candidate
        let mut pruned = 0;
        for candidate in &candidates {
            // Stop and remove container
            if let Some(ref container_id) = candidate.container_id {
                let _ = client.stop_container(container_id).await;
                let _ = client.remove_container(container_id, false).await;
                info!(
                    branch = %candidate.branch,
                    container = candidate.container_name.as_deref().unwrap_or("-"),
                    "removed container"
                );
            }

            // Remove worktree
            match cella_git::remove(repo_root, &candidate.worktree_path) {
                Ok(()) => {
                    eprintln!(
                        "  Pruned: {} ({})",
                        candidate.branch,
                        if candidate.container_id.is_some() {
                            "container removed"
                        } else {
                            "no container"
                        }
                    );
                    pruned += 1;
                }
                Err(e) => {
                    eprintln!("  Failed to remove worktree for {}: {e}", candidate.branch);
                }
            }
        }

        // 10. Clean up stale git worktree records
        let _ = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_root)
            .output();

        eprintln!(
            "\nPruned {pruned} worktree{}",
            if pruned == 1 { "" } else { "s" }
        );
        Ok(())
    }
}

fn format_candidates(candidates: &[PruneCandidate]) -> String {
    let mut buf = format!(
        "{:<20} {:<44} {:<24} STATE\n",
        "BRANCH", "WORKTREE", "CONTAINER"
    );
    for c in candidates {
        let _ = writeln!(
            buf,
            "{:<20} {:<44} {:<24} {}",
            c.branch,
            c.worktree_path.display(),
            c.container_name.as_deref().unwrap_or("-"),
            c.container_state.as_deref().unwrap_or("-"),
        );
    }
    buf
}

fn print_candidates(candidates: &[PruneCandidate]) {
    eprint!("{}", format_candidates(candidates));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn format_candidates_with_containers() {
        let candidates = vec![
            PruneCandidate {
                branch: "fix/typo".to_string(),
                worktree_path: PathBuf::from("/workspaces/cella-worktrees/fix-typo"),
                container_name: Some("cella-fix-typo-abc12".to_string()),
                container_id: Some("abc12345".to_string()),
                container_state: Some("stopped".to_string()),
            },
            PruneCandidate {
                branch: "feat/old".to_string(),
                worktree_path: PathBuf::from("/workspaces/cella-worktrees/feat-old"),
                container_name: Some("cella-feat-old-def34".to_string()),
                container_id: Some("def34567".to_string()),
                container_state: Some("running".to_string()),
            },
        ];

        let output = format_candidates(&candidates);
        insta::assert_snapshot!(output, @r"
        BRANCH               WORKTREE                                     CONTAINER                STATE
        fix/typo             /workspaces/cella-worktrees/fix-typo         cella-fix-typo-abc12     stopped
        feat/old             /workspaces/cella-worktrees/feat-old         cella-feat-old-def34     running
        ");
    }

    #[test]
    fn format_candidates_without_containers() {
        let candidates = vec![PruneCandidate {
            branch: "orphan".to_string(),
            worktree_path: PathBuf::from("/worktrees/orphan"),
            container_name: None,
            container_id: None,
            container_state: None,
        }];

        let output = format_candidates(&candidates);
        insta::assert_snapshot!(output, @r"
        BRANCH               WORKTREE                                     CONTAINER                STATE
        orphan               /worktrees/orphan                            -                        -
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
}
