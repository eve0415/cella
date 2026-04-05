use clap::Args;
use tracing::debug;

use cella_backend::{ContainerTarget, InteractiveExecOptions};

use crate::picker;

/// Switch to a different worktree-backed branch (opens a shell in its container).
#[derive(Args)]
pub struct SwitchArgs {
    /// Name of the branch to switch to (interactive picker if omitted).
    pub name: Option<String>,

    /// Shell to use (e.g., bash, zsh, fish).
    #[arg(short, long)]
    shell: Option<String>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,
}

impl SwitchArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;

        // Discover repo and list worktrees
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd)?;
        let worktrees = cella_git::list(&repo_info.root)?;

        // Build branch → container state map for picker display
        let containers = client.as_ref().list_cella_containers(false).await?;
        let container_states = picker::branch_container_states(&containers);

        // Resolve branch interactively (exact match, picker, or pre-filtered)
        let wt = picker::resolve_worktree_interactive(
            &worktrees,
            &container_states,
            self.name.as_deref(),
            repo_info.head_branch.as_deref(),
        )?;

        let workspace_folder = wt.path;

        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: Some(workspace_folder),
        };

        let container = target.resolve(client.as_ref(), true).await?;

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
            cella_orchestrator::shell_detect::detect_shell(client.as_ref(), &container.id, &user)
                .await
        };

        debug!("Using shell: {shell}");

        // Build environment: probed env (merged with label env) + terminal env
        let base_env = if let Some(probed) = cella_orchestrator::env_cache::read_probed_env_cache(
            client.as_ref(),
            &container.id,
            &user,
        )
        .await
        {
            cella_env::user_env_probe::merge_env(&probed, &label_env)
        } else {
            label_env
        };
        let mut env = base_env;

        // SSH_AUTH_SOCK fallback for containers created before forwarding env was stored
        cella_orchestrator::env_cache::ensure_ssh_auth_sock(
            client.as_ref(),
            &container.id,
            &user,
            &mut env,
        )
        .await;

        for var in super::TERMINAL_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                env.push(format!("{var}={val}"));
            }
        }

        let exit_code = client
            .as_ref()
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
