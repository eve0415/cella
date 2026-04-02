use std::io::{self, BufRead, Write};

use clap::Args;

use std::future::Future;
use std::pin::Pin;

use cella_orchestrator::prune::{PruneCandidate, PruneHooks};

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

        // 2. Build candidates via orchestrator
        let client = super::resolve_backend_for_command(backend, self.docker_host.as_deref())?;
        if !self.is_json() {
            eprintln!("Fetching remote refs...");
        }
        let candidates =
            cella_orchestrator::prune::build_prune_candidates(repo_root, client.as_ref(), self.all)
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

        // 3. Display, confirm, and execute
        self.confirm_and_prune(candidates, client.as_ref(), repo_root)
            .await
    }

    async fn confirm_and_prune(
        &self,
        candidates: Vec<PruneCandidate>,
        client: &dyn cella_backend::ContainerBackend,
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

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let progress = cella_orchestrator::ProgressSender::new(tx, false);
        let hooks = CliPruneHooks;
        let result = cella_orchestrator::prune::execute_prune(
            repo_root,
            client,
            &candidates,
            &progress,
            &hooks,
        )
        .await;

        if self.is_json() {
            let names: Vec<&str> = result.pruned.iter().map(|e| e.branch.as_str()).collect();
            print_json_result(&names, &result.errors);
        } else {
            for err in &result.errors {
                eprintln!("error: {err}");
            }
            let count = result.pruned.len();
            eprintln!(
                "\nPruned {count} worktree{}",
                if count == 1 { "" } else { "s" }
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CLI-side PruneHooks implementation
// ---------------------------------------------------------------------------

struct CliPruneHooks;

impl PruneHooks for CliPruneHooks {
    fn deregister_container(
        &self,
        container: &cella_backend::ContainerInfo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let container = container.clone();
        Box::pin(async move {
            super::down::deregister_container(&container).await;
        })
    }

    fn compose_down(
        &self,
        project_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let project = project_name.to_string();
        Box::pin(async move {
            let compose_cmd = cella_compose::ComposeCommand::from_project_name(&project);
            compose_cmd
                .down()
                .await
                .map_err(|e| format!("docker compose down failed: {e}"))
        })
    }

    fn cleanup_daemon(&self) {
        super::down::cleanup_daemon();
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

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

    use cella_backend::{BackendKind, ContainerInfo, ContainerState};
    use cella_orchestrator::prune::PruneReason;

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
            backend: BackendKind::Docker,
        }
    }

    #[test]
    fn format_candidates_with_containers() {
        let candidates = vec![
            PruneCandidate {
                branch: "fix/typo".to_string(),
                worktree_path: std::path::PathBuf::from("/workspaces/cella-worktrees/fix-typo"),
                container: Some(make_container(
                    "cella-fix-typo-abc12",
                    "abc12345",
                    ContainerState::Stopped,
                )),
                reason: PruneReason::Merged,
            },
            PruneCandidate {
                branch: "feat/old".to_string(),
                worktree_path: std::path::PathBuf::from("/workspaces/cella-worktrees/feat-old"),
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
            worktree_path: std::path::PathBuf::from("/worktrees/orphan"),
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
        assert_eq!(output.lines().count(), 1);
        assert!(output.starts_with("BRANCH"));
    }

    #[test]
    fn format_candidates_long_paths() {
        let candidates = vec![PruneCandidate {
            branch: "feat/a-very-long-branch-name".to_string(),
            worktree_path: std::path::PathBuf::from(
                "/very/long/worktree/path/feat-a-very-long-branch-name",
            ),
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
        print_json_result(&[], &[]);
    }

    #[test]
    fn print_json_result_with_errors() {
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
