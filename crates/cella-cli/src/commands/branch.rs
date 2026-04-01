use clap::Args;
use tracing::info;

use super::up::{OutputFormat, UpContext};
use cella_docker::{ExecOptions, worktree_labels};
use cella_git::WorktreeInfo;

/// Create a new worktree-backed branch with its own dev container.
#[derive(Args)]
pub struct BranchArgs {
    #[command(flatten)]
    pub verbose: super::VerboseArgs,

    /// Name for the new branch (or existing branch to check out).
    pub name: String,

    /// Base ref to branch from (defaults to HEAD). Only for new branches.
    #[arg(long)]
    pub base: Option<String>,

    /// Command to execute in the new container after creation.
    #[arg(long = "exec")]
    pub exec_cmd: Option<String>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

impl BranchArgs {
    pub async fn execute(
        self,
        progress: crate::progress::Progress,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = backend; // TODO: pass through to UpContext once it accepts backend
        // 1. Discover git repo
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd).map_err(|e| -> Box<dyn std::error::Error> {
            format!("Not inside a git repository: {e}").into()
        })?;
        let repo_root = &repo_info.root;

        // 2. Resolve branch state
        let branch_state = cella_git::resolve_branch(repo_root, &self.name)?;
        info!(
            branch = %self.name,
            state = ?branch_state,
            "resolved branch"
        );

        // 3. Create git worktree
        let step = progress.step(&format!("Creating worktree for '{}'...", self.name));
        let wt_info = match cella_git::create(
            repo_root,
            &self.name,
            &branch_state,
            None, // worktree_root config — will read from cella.toml when field is added
            self.base.as_deref(),
        ) {
            Ok(info) => {
                step.finish();
                info
            }
            Err(cella_git::CellaGitError::WorktreeAlreadyExists { path }) => {
                step.fail("already exists");
                return Err(format!(
                    "Worktree for '{}' already exists at {}\n\
                     Use `cella switch {}` to switch to it, or \
                     `cella up --workspace-folder {}` to start its container.",
                    self.name,
                    path.display(),
                    self.name,
                    path.display(),
                )
                .into());
            }
            Err(cella_git::CellaGitError::BranchCheckedOut {
                branch,
                worktree_path,
            }) => {
                step.fail("branch in use");
                return Err(format!(
                    "Branch '{branch}' is already checked out at {}",
                    worktree_path.display(),
                )
                .into());
            }
            Err(e) => {
                step.fail("failed");
                return Err(e.into());
            }
        };

        // 4. Run container pipeline (with rollback on failure)
        let result = self
            .run_container_pipeline(&wt_info, repo_root, &progress)
            .await;

        if let Err(e) = &result {
            // Rollback: remove the worktree on container failure
            progress.warn(&format!("Container creation failed: {e}"));
            let rollback_step = progress.step("Rolling back worktree...");
            if let Err(re) = cella_git::remove(repo_root, &wt_info.path) {
                rollback_step.fail("rollback failed");
                progress.warn(&format!("Failed to remove worktree: {re}"));
            } else {
                rollback_step.finish();
            }
        }

        result
    }

    async fn run_container_pipeline(
        &self,
        wt_info: &WorktreeInfo,
        repo_root: &std::path::Path,
        progress: &crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Prepare worktree-specific labels
        let extra_labels = worktree_labels(&self.name, repo_root);

        // Create the container using the up pipeline
        let ctx = UpContext::for_workspace(
            &wt_info.path,
            self.docker_host.as_deref(),
            extra_labels,
            progress.clone(),
            self.output.clone(),
        )
        .await?;

        let create_result = ctx.create_and_start(false).await?;

        // If --exec provided, run the command in the new container
        if let Some(ref exec_cmd) = self.exec_cmd {
            let step = progress.step(&format!("Executing: {exec_cmd}"));
            let exec_result = ctx
                .client
                .exec_command(
                    &create_result.container_id,
                    &ExecOptions {
                        cmd: vec!["sh".to_string(), "-c".to_string(), exec_cmd.clone()],
                        user: Some(create_result.remote_user.clone()),
                        working_dir: None,
                        env: None,
                    },
                )
                .await?;

            if exec_result.exit_code != 0 {
                step.fail(&format!("exit code {}", exec_result.exit_code));
            } else {
                step.finish();
            }
        }

        // Summary
        match self.output {
            OutputFormat::Text => {
                eprintln!(
                    "Ready: {} (container: {})",
                    wt_info.path.display(),
                    ctx.container_nm,
                );
            }
            OutputFormat::Json => {
                let output = serde_json::json!({
                    "containerId": create_result.container_id,
                    "containerName": ctx.container_nm,
                    "worktreePath": wt_info.path.display().to_string(),
                    "branch": self.name,
                });
                println!("{}", serde_json::to_string(&output).unwrap_or_default());
            }
        }

        Ok(())
    }
}
