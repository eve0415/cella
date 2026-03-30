use std::path::PathBuf;

use clap::Args;
use tracing::debug;

use cella_docker::{ContainerTarget, InteractiveExecOptions};

/// Switch to a different worktree-backed branch (opens a shell in its container).
#[derive(Args)]
pub struct SwitchArgs {
    /// Name of the branch to switch to.
    pub name: String,

    /// Shell to use (e.g., bash, zsh, fish).
    #[arg(short, long)]
    shell: Option<String>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,
}

impl SwitchArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        // Resolve branch name to worktree path
        let workspace_folder = resolve_branch_to_path(&self.name)?;

        let client = super::connect_docker(self.docker_host.as_deref())?;

        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: Some(workspace_folder),
        };

        let container = target.resolve(&client, true).await?;

        super::ensure_cella_daemon().await;

        // Read exec metadata from container labels
        let user = container
            .labels
            .get("dev.cella.remote_user")
            .cloned()
            .or_else(|| container.container_user.clone())
            .unwrap_or_else(|| "root".to_string());

        let working_dir = container.labels.get("dev.cella.workspace_folder").cloned();

        let label_env: Vec<String> = container
            .labels
            .get("dev.cella.remote_env")
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();

        // Detect shell
        let shell = if let Some(s) = self.shell {
            s
        } else {
            super::shell_detect::detect_shell(&client, &container.id, &user).await
        };

        debug!("Using shell: {shell}");

        // Build environment: probed env (merged with label env) + terminal env
        let base_env = if let Some(probed) =
            super::env_cache::read_probed_env_cache(&client, &container.id, &user).await
        {
            cella_env::user_env_probe::merge_env(&probed, &label_env)
        } else {
            label_env
        };
        let mut env = base_env;

        // SSH_AUTH_SOCK fallback for containers created before forwarding env was stored
        super::env_cache::ensure_ssh_auth_sock(&client, &container.id, &user, &mut env).await;

        for var in super::TERMINAL_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                env.push(format!("{var}={val}"));
            }
        }

        let exit_code = client
            .exec_interactive(
                &container.id,
                &InteractiveExecOptions {
                    cmd: vec![shell, "-l".to_string()],
                    user: Some(user),
                    env: Some(env),
                    working_dir,
                    tty: true,
                },
            )
            .await?;

        std::process::exit(i32::try_from(exit_code).unwrap_or(125));
    }
}

/// Resolve a branch name to its worktree path.
fn resolve_branch_to_path(branch_name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let repo_info = cella_git::discover(&cwd)?;
    let worktrees = cella_git::list(&repo_info.root)?;
    let wt = worktrees
        .iter()
        .find(|wt| wt.branch.as_deref() == Some(branch_name))
        .ok_or_else(|| format!("No worktree found for branch '{branch_name}'"))?;
    Ok(wt.path.clone())
}
