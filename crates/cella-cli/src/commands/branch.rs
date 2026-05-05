use clap::Args;

use super::OutputFormat;
use super::up::UpContext;
use cella_backend::{ExecOptions, worktree_labels};

/// Create a new worktree-backed branch with its own dev container.
///
/// Idempotent: if the worktree already exists for the named branch, ensures
/// the container is running rather than erroring.
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

    /// Error if the worktree already exists (disable idempotent behavior).
    #[arg(long)]
    pub fail_if_exists: bool,

    /// Custom labels to add to the container (repeatable, KEY=VALUE format).
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

impl BranchArgs {
    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 1. Discover git repo
        let cwd = std::env::current_dir()?;
        let repo_info =
            cella_git::discover(&cwd).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("Not inside a git repository: {e}").into()
            })?;
        let repo_root = &repo_info.root;

        // 2. Acquire per-branch advisory lock for concurrent safety
        let _lock = cella_git::BranchLock::acquire(repo_root, &self.name).map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("Failed to acquire branch lock: {e}").into()
            },
        )?;

        // 3. Create git worktree via orchestrator (idempotent)
        let (sender, renderer) = crate::progress::bridge(&progress);
        let wt_result = cella_orchestrator::branch::create_worktree(
            repo_root,
            &self.name,
            self.base.as_deref(),
            None,
            &sender,
        )
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        drop(sender);
        let _ = renderer.await;

        if !wt_result.created && self.fail_if_exists {
            return Err(format!(
                "Worktree already exists for branch '{}' at {}",
                self.name,
                wt_result.path.display()
            )
            .into());
        }

        // 4. Run container pipeline (with rollback only for freshly-created worktrees)
        let result = self
            .run_container_pipeline(&wt_result.path, repo_root, wt_result.created, &progress)
            .await;

        if let Err(e) = &result
            && wt_result.created
        {
            progress.warn(&format!("Container creation failed: {e}"));
            let rollback_step = progress.step("Rolling back worktree...");
            if let Err(re) = cella_git::remove(repo_root, &wt_result.path) {
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
        freshly_created: bool,
        progress: &crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !freshly_created && !self.labels.is_empty() {
            progress.warn(
                "Branch already exists, --label flags ignored (container labels are immutable)",
            );
        }

        // Prepare worktree-specific labels + user-supplied labels
        let mut extra_labels = worktree_labels(&self.name, repo_root);
        for label in &self.labels {
            if let Some((key, value)) = label.split_once('=') {
                if key.starts_with("dev.cella.") || key.starts_with("devcontainer.") {
                    return Err(format!(
                        "reserved label prefix: '{key}' — dev.cella.* and devcontainer.* labels are managed by cella"
                    ).into());
                }
                extra_labels.insert(key.to_string(), value.to_string());
            }
        }

        // Create the container using the up pipeline
        let mut ctx = UpContext::for_workspace(
            wt_path,
            &self.backend,
            extra_labels,
            progress.clone(),
            self.output.clone(),
        )
        .await?;
        let _title_guard = crate::title::push_for_workspace(
            ctx.client.as_ref(),
            wt_path,
            &ctx.container_nm,
            None,
            Some(&self.name),
            "branch",
        )
        .await;

        if freshly_created {
            remove_leftover_container(&ctx, wt_path).await;
        }

        let create_result = if ctx.is_compose() {
            run_compose_branch(&mut ctx, repo_root, progress, false, &[]).await?
        } else {
            ctx.ensure_up(false, &[]).await?
        };

        // If --exec provided, run the command in the new container
        let exec_exit_code = if let Some(ref exec_cmd) = self.exec_cmd {
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
            Some(exec_result.exit_code)
        } else {
            None
        };

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
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output).unwrap_or_default()
                );
            }
        }

        // Propagate --exec exit code to the caller
        if let Some(code) = exec_exit_code
            && code != 0
        {
            std::process::exit(i32::try_from(code).unwrap_or(125));
        }

        Ok(())
    }
}

/// Remove leftover container from a previous failed attempt so `ensure_up`
/// runs the full first-create path (lifecycle hooks, tool setup, etc.).
async fn remove_leftover_container(ctx: &UpContext, wt_path: &std::path::Path) {
    if let Ok(Some(existing)) = ctx.client.find_container(wt_path).await {
        if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
            && mgmt_sock.exists()
        {
            let req = cella_protocol::ManagementRequest::DeregisterContainer {
                container_name: existing.name.clone(),
            };
            let _ = cella_daemon_client::send_management_request(&mgmt_sock, &req).await;
        }
        let _ = ctx.client.stop_container(&existing.id).await;
        let _ = ctx.client.remove_container(&existing.id, false).await;
    }
}

/// Build compose image, create a standalone container, and connect to the
/// parent project's compose network.
///
/// Used by both `cella branch` and `cella up --branch` for compose projects.
pub(super) async fn run_compose_branch(
    ctx: &mut UpContext,
    repo_root: &std::path::Path,
    progress: &crate::progress::Progress,
    build_no_cache: bool,
    strict: &[super::StrictnessLevel],
) -> Result<super::up::UpResult, Box<dyn std::error::Error + Send + Sync>> {
    let project = cella_compose::ComposeProject::from_resolved(
        ctx.config(),
        &ctx.resolved.config_path,
        &ctx.resolved.workspace_root,
    )?;
    let service = project.primary_service.clone();

    let step = progress.step("Building compose image...");
    let compose_cmd = cella_compose::ComposeCommand::without_override(&project);
    compose_cmd
        .build(Some(std::slice::from_ref(&service)), build_no_cache)
        .await?;
    step.finish();

    let resolved_compose = compose_cmd.config().await?;
    let build_info = cella_compose::extract_service_build_info(&resolved_compose, &service)?;
    let is_local_image = matches!(&build_info, cella_compose::ServiceBuildInfo::Build { .. });
    let image_name = match &build_info {
        cella_compose::ServiceBuildInfo::Image { image } => image.clone(),
        cella_compose::ServiceBuildInfo::Build { .. } => {
            format!("{}-{}", project.project_name, service)
        }
    };

    let parent_config_name = ctx.resolved.name().map(str::to_string);
    let parent_project_name =
        cella_backend::compose_project_name(repo_root, parent_config_name.as_deref());
    let parent_network = format!("{parent_project_name}_default");

    let network_ok = ctx
        .client
        .network_exists(&parent_network)
        .await
        .unwrap_or(false);
    if !network_ok {
        progress.warn(&format!(
            "Parent compose network '{parent_network}' not found. \
             Run `cella up` first to create it."
        ));
    }

    // Swap the config to an image-based one so ensure_up creates a standalone
    // container instead of rejecting the compose config.
    let obj = ctx
        .resolved
        .config
        .as_object_mut()
        .expect("config is an object");
    obj.remove("dockerComposeFile");
    obj.remove("service");
    obj.insert("image".to_string(), serde_json::Value::String(image_name));

    // Register the parent compose network so the orchestrator connects
    // the container before lifecycle hooks run.
    if network_ok {
        ctx.extra_networks.push(parent_network);
    }

    if build_no_cache {
        ctx.remove_container = true;
    }
    // Local-only images (from build:) must not be pulled. Registry
    // images (from image:) need normal pull behavior.
    if is_local_image {
        ctx.pull_policy = Some("never".to_string());
    }
    ctx.ensure_up(build_no_cache, strict).await
}
