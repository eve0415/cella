use clap::Args;

use super::up::{OutputFormat, UpContext};
use cella_backend::{ExecOptions, worktree_labels};

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
        // 1. Discover git repo
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd).map_err(|e| -> Box<dyn std::error::Error> {
            format!("Not inside a git repository: {e}").into()
        })?;
        let repo_root = &repo_info.root;

        // 2. Create git worktree via orchestrator
        let (sender, renderer) = crate::progress::bridge(&progress);
        let wt_path = cella_orchestrator::branch::create_worktree(
            repo_root,
            &self.name,
            self.base.as_deref(),
            None,
            &sender,
        )
        .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
        drop(sender);
        let _ = renderer.await;

        // 3. Run container pipeline (with rollback on failure)
        let result = self
            .run_container_pipeline(&wt_path, repo_root, &progress, backend)
            .await;

        if let Err(e) = &result {
            // Rollback: remove the worktree on container failure
            progress.warn(&format!("Container creation failed: {e}"));
            let rollback_step = progress.step("Rolling back worktree...");
            if let Err(re) = cella_git::remove(repo_root, &wt_path) {
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
        wt_path: &std::path::Path,
        repo_root: &std::path::Path,
        progress: &crate::progress::Progress,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Prepare worktree-specific labels
        let extra_labels = worktree_labels(&self.name, repo_root);

        // Create the container using the up pipeline
        let ctx = UpContext::for_workspace(
            wt_path,
            self.docker_host.as_deref(),
            extra_labels,
            progress.clone(),
            self.output.clone(),
            backend,
        )
        .await?;

        // Remove any leftover container from a previous failed attempt so
        // ensure_up always runs the full first-create path (lifecycle hooks,
        // tool setup, etc.) rather than reusing a half-initialized container.
        if let Ok(Some(existing)) = ctx.client.find_container(wt_path).await {
            // Deregister from daemon to clean up forwarded ports
            if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
                && mgmt_sock.exists()
            {
                let req = cella_protocol::ManagementRequest::DeregisterContainer {
                    container_name: existing.name.clone(),
                };
                let _ = cella_daemon::management::send_management_request(&mgmt_sock, &req).await;
            }
            let _ = ctx.client.stop_container(&existing.id).await;
            let _ = ctx.client.remove_container(&existing.id, false).await;
        }

        let create_result = ctx.ensure_up(false, &[]).await?;

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
                    wt_path.display(),
                    ctx.container_nm,
                );
            }
            OutputFormat::Json => {
                let output = serde_json::json!({
                    "containerId": create_result.container_id,
                    "containerName": ctx.container_nm,
                    "worktreePath": wt_path.display().to_string(),
                    "branch": self.name,
                });
                println!("{}", serde_json::to_string(&output).unwrap_or_default());
            }
        }

        Ok(())
    }
}
