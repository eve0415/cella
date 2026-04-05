use std::path::PathBuf;

use clap::Args;
use tracing::debug;

use cella_backend::{ContainerTarget, InteractiveExecOptions};

use crate::picker;

/// Open a shell inside the running dev container.
#[derive(Args)]
pub struct ShellArgs {
    /// Shell to use (e.g., bash, zsh, fish).
    #[arg(short, long)]
    shell: Option<String>,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Target container by ID.
    #[arg(long)]
    container_id: Option<String>,

    /// Target container by name.
    #[arg(long)]
    container_name: Option<String>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    service: Option<String>,
}

impl ShellArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: None,
            workspace_folder: self.workspace_folder,
        };

        let has_explicit = picker::has_explicit_target(&target);
        let container = match target.resolve(client.as_ref(), true).await {
            Ok(c) => c,
            Err(_) if !has_explicit => {
                let containers = client.as_ref().list_cella_containers(true).await?;
                let cwd_container = client
                    .as_ref()
                    .find_container(&std::env::current_dir()?)
                    .await
                    .ok()
                    .flatten();
                picker::resolve_container_interactive(
                    &containers,
                    cwd_container.as_ref().map(|c| c.name.as_str()),
                    "Select a container:",
                    None,
                )?
            }
            Err(e) => return Err(e.into()),
        };
        let container =
            super::resolve_service_container(client.as_ref(), container, self.service.as_deref())
                .await?;

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
