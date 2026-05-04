use std::io::{self, BufRead, Write};

use clap::Args;

use std::future::Future;
use std::pin::Pin;

use cella_backend::{ManagedNetwork, RemovalOutcome};
use cella_orchestrator::prune::{PruneCandidate, PruneHooks};

use super::OutputFormat;

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

    #[command(flatten)]
    scope: PruneScope,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// Output format (text or json).
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

/// Mutually-exclusive scope selectors for `cella prune`.
///
/// Separated from [`PruneArgs`] to keep its bool-field count under
/// `clippy::struct_excessive_bools`.
#[derive(Args)]
struct PruneScope {
    /// Include unmerged worktrees (not just merged ones).
    #[arg(long)]
    all: bool,

    /// Sweep cella-managed Docker networks with zero attached containers.
    ///
    /// In this mode worktrees are NOT touched. Scans the host for every
    /// network labeled `dev.cella.managed=true` and removes those that
    /// no container is currently attached to. Safe: never force-
    /// disconnects endpoints.
    #[arg(long, conflicts_with = "all")]
    networks: bool,

    /// Only prune worktrees/containers older than this duration (e.g. 2h, 7d).
    #[arg(long)]
    older_than: Option<String>,

    /// Prune containers whose workspace path no longer exists on disk.
    #[arg(long)]
    missing_worktree: bool,

    /// Only prune containers matching this label (KEY=VALUE, repeatable).
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,
}

impl PruneArgs {
    const fn is_json(&self) -> bool {
        matches!(self.output, OutputFormat::Json)
    }

    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.scope.networks {
            return self.execute_networks_sweep().await;
        }

        // 1. Discover git repo
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd)?;
        let repo_root = &repo_info.root;

        // 2. Build candidates via orchestrator
        let client = self.backend.resolve_client().await?;
        if !self.is_json() {
            eprintln!("Fetching remote refs...");
        }

        let include_all = self.scope.all || self.scope.missing_worktree;
        let mut candidates = cella_orchestrator::prune::build_prune_candidates(
            repo_root,
            client.as_ref(),
            include_all,
        )
        .await?;

        // 3. Validate filter args
        for spec in &self.scope.labels {
            if !spec.contains('=') {
                return Err(
                    format!("invalid label filter '{spec}': expected KEY=VALUE format").into(),
                );
            }
        }

        let older_than = self
            .scope
            .older_than
            .as_deref()
            .map(parse_duration)
            .transpose()?;

        candidates.retain(|c| {
            if let Some(max_age) = older_than {
                let created = c
                    .container
                    .as_ref()
                    .and_then(|ci| ci.created_at.as_deref())
                    .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok());
                if let Some(dt) = created {
                    let age = chrono::Utc::now().signed_duration_since(dt);
                    if age < max_age {
                        return false;
                    }
                }
            }

            if self.scope.missing_worktree && c.worktree_path.exists() {
                return false;
            }

            if !self.scope.labels.is_empty() {
                let Some(ci) = &c.container else {
                    return false;
                };
                let all_match = self.scope.labels.iter().all(|spec| {
                    spec.split_once('=')
                        .is_some_and(|(k, v)| ci.labels.get(k).is_some_and(|lv| lv == v))
                });
                if !all_match {
                    return false;
                }
            }

            true
        });

        if candidates.is_empty() {
            if self.is_json() {
                print_json_result(&[], &[]);
            } else if self.scope.all {
                eprintln!("Nothing to prune. No linked worktrees found.");
            } else {
                eprintln!("Nothing to prune. No matching worktrees found.");
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
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            let unmerged = if self.scope.all {
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

    async fn execute_networks_sweep(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;
        let all = client.list_managed_networks().await?;
        let orphans: Vec<ManagedNetwork> =
            all.into_iter().filter(|n| n.container_count == 0).collect();

        if orphans.is_empty() {
            if self.is_json() {
                print_networks_json_result(&[], &[], &[]);
            } else {
                eprintln!("Nothing to prune. No orphan cella networks found.");
            }
            return Ok(());
        }

        if !self.is_json() {
            eprint!("{}", format_orphan_networks(&orphans));
        }

        if self.dry_run {
            if self.is_json() {
                let would: Vec<&str> = orphans.iter().map(|n| n.name.as_str()).collect();
                print_networks_json_result(&would, &[], &[]);
            } else {
                eprintln!("\nDry run — no changes made.");
            }
            return Ok(());
        }

        if !self.force && !self.is_json() {
            let plural = if orphans.len() == 1 { "" } else { "s" };
            eprint!("\nRemove {} network{plural}? [y/N] ", orphans.len());
            io::stderr().flush()?;

            let stdin = io::stdin();
            let mut line = String::new();
            stdin.lock().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                eprintln!("Aborted.");
                return Ok(());
            }
        }

        let mut removed = Vec::new();
        let mut skipped = Vec::new();
        let mut errors = Vec::new();
        for net in &orphans {
            match client.remove_network_if_orphan(&net.name).await {
                // NotFound means another caller beat us to it — same end
                // state, treat as success.
                Ok(RemovalOutcome::Removed | RemovalOutcome::NotFound) => {
                    removed.push(net.name.clone());
                }
                Ok(RemovalOutcome::SkippedInUse) => skipped.push(net.name.clone()),
                Err(e) => errors.push(format!("{}: {e}", net.name)),
            }
        }

        if self.is_json() {
            let removed_refs: Vec<&str> = removed.iter().map(String::as_str).collect();
            let skipped_refs: Vec<&str> = skipped.iter().map(String::as_str).collect();
            print_networks_json_result(&removed_refs, &skipped_refs, &errors);
        } else {
            for err in &errors {
                eprintln!("error: {err}");
            }
            let count = removed.len();
            let plural = if count == 1 { "" } else { "s" };
            eprintln!("\nRemoved {count} network{plural}");
            if !skipped.is_empty() {
                eprintln!(
                    "Skipped {} in-use network{}",
                    skipped.len(),
                    if skipped.len() == 1 { "" } else { "s" }
                );
            }
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

fn print_networks_json_result(removed: &[&str], skipped: &[&str], errors: &[String]) {
    let result = serde_json::json!({
        "removed": removed,
        "skipped": skipped,
        "errors": errors,
    });
    println!("{result}");
}

fn format_orphan_networks(networks: &[ManagedNetwork]) -> String {
    use crate::table::{Column, Table};

    let mut table = Table::new(vec![
        Column::fixed("NAME"),
        Column::shrinkable("REPO"),
        Column::fixed("CREATED"),
    ]);

    for net in networks {
        table.add_row(vec![
            net.name.clone(),
            net.repo_path.clone().unwrap_or_else(|| "-".to_string()),
            net.created_at.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }

    table.render()
}

/// Parse a human-readable duration like "2h", "7d", "30m" into a `chrono::Duration`.
fn parse_duration(s: &str) -> Result<chrono::Duration, Box<dyn std::error::Error + Send + Sync>> {
    let s = s.trim();
    let (num, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let value: i64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
    match suffix {
        "s" => Ok(chrono::Duration::seconds(value)),
        "m" => Ok(chrono::Duration::minutes(value)),
        "h" => Ok(chrono::Duration::hours(value)),
        "d" => Ok(chrono::Duration::days(value)),
        _ => Err(format!("unknown duration suffix '{suffix}', expected s/m/h/d").into()),
    }
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

    // -----------------------------------------------------------------------
    // --networks flag and sweep output tests
    // -----------------------------------------------------------------------

    fn make_network(name: &str, repo: Option<&str>, created: Option<&str>) -> ManagedNetwork {
        ManagedNetwork {
            name: name.to_string(),
            repo_path: repo.map(String::from),
            container_count: 0,
            created_at: created.map(String::from),
            labels: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn prune_args_networks_flag_parses() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "prune", "--networks"]).unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert!(args.scope.networks);
            assert!(!args.scope.all);
        } else {
            panic!("expected Prune subcommand");
        }
    }

    #[test]
    fn prune_args_networks_conflicts_with_all() {
        use clap::Parser;
        let result = crate::Cli::try_parse_from(["cella", "prune", "--networks", "--all"]);
        assert!(
            result.is_err(),
            "--networks and --all must be mutually exclusive"
        );
    }

    #[test]
    fn prune_args_networks_default_false() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "prune"]).unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert!(!args.scope.networks);
        } else {
            panic!("expected Prune subcommand");
        }
    }

    #[test]
    fn format_orphan_networks_with_repo_and_timestamp() {
        let networks = vec![
            make_network(
                "cella-net-abcdef123456",
                Some("/workspaces/cella"),
                Some("2026-04-01T12:00:00Z"),
            ),
            make_network("cella", None, Some("2026-03-15T08:30:00Z")),
        ];
        let output = format_orphan_networks(&networks);
        insta::assert_snapshot!(output, @"
        NAME                    REPO               CREATED
        cella-net-abcdef123456  /workspaces/cella  2026-04-01T12:00:00Z
        cella                   -                  2026-03-15T08:30:00Z
        ");
    }

    #[test]
    fn format_orphan_networks_missing_metadata() {
        let networks = vec![make_network("cella-net-deadbeef0000", None, None)];
        let output = format_orphan_networks(&networks);
        assert!(output.contains("cella-net-deadbeef0000"));
        assert!(
            output.contains(" -   ") || output.contains(" - "),
            "missing metadata should render as '-': {output}"
        );
    }

    #[test]
    fn format_orphan_networks_empty() {
        let networks: Vec<ManagedNetwork> = vec![];
        let output = format_orphan_networks(&networks);
        assert_eq!(output.lines().count(), 1);
        assert!(output.starts_with("NAME"));
    }

    #[test]
    fn print_networks_json_result_empty() {
        print_networks_json_result(&[], &[], &[]);
    }

    #[test]
    fn print_networks_json_result_with_all_categories() {
        print_networks_json_result(
            &["cella-net-aaa"],
            &["cella"],
            &["cella-net-bbb: permission denied".to_string()],
        );
    }

    // ── parse_duration ────────────────────────────────────────────

    #[test]
    fn parse_duration_seconds() {
        let d = parse_duration("30s").unwrap();
        assert_eq!(d.num_seconds(), 30);
    }

    #[test]
    fn parse_duration_minutes() {
        let d = parse_duration("5m").unwrap();
        assert_eq!(d.num_minutes(), 5);
    }

    #[test]
    fn parse_duration_hours() {
        let d = parse_duration("2h").unwrap();
        assert_eq!(d.num_hours(), 2);
    }

    #[test]
    fn parse_duration_days() {
        let d = parse_duration("7d").unwrap();
        assert_eq!(d.num_days(), 7);
    }

    #[test]
    fn parse_duration_invalid_number() {
        assert!(parse_duration("xh").is_err());
    }

    #[test]
    fn parse_duration_unknown_suffix() {
        assert!(parse_duration("5w").is_err());
    }

    // ── filter flag parsing ───────────────────────────────────────

    #[test]
    fn prune_args_older_than_parses() {
        use clap::Parser;
        let cli =
            crate::Cli::try_parse_from(["cella", "prune", "--older-than", "2h", "--all"]).unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert_eq!(args.scope.older_than.as_deref(), Some("2h"));
        }
    }

    #[test]
    fn prune_args_missing_worktree_parses() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "prune", "--missing-worktree"]).unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert!(args.scope.missing_worktree);
        }
    }

    #[test]
    fn prune_args_label_parses() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "cella",
            "prune",
            "--label",
            "session=abc",
            "--label",
            "agent=claude",
            "--all",
        ])
        .unwrap();
        if let crate::commands::Command::Prune(args) = &cli.command {
            assert_eq!(args.scope.labels.len(), 2);
            assert_eq!(args.scope.labels[0], "session=abc");
        }
    }
}
