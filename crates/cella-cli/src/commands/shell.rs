use std::path::PathBuf;

use clap::Args;
use tracing::{debug, warn};

use cella_backend::{ContainerTarget, InteractiveExecOptions};
use cella_orchestrator::shell_detect::{ShellResolution, ShellSource};

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

        let container = if self.container_id.is_some() || self.container_name.is_some() {
            // Explicit id target wins: --container-id or --container-name.
            // (shell has no --id-label flag, so those are the only id targets.)
            let target = ContainerTarget {
                container_id: self.container_id,
                container_name: self.container_name,
                id_labels: Vec::new(),
                workspace_folder: self.workspace_folder,
            };
            target.resolve(client.as_ref(), true).await?
        } else {
            // No id target: resolve workspace + default config, then use spec-identity
            // lookup with legacy fallback. An explicit --workspace-folder errors on
            // a miss (the user targeted a specific environment); a bare invocation
            // falls to the interactive picker.
            let has_explicit_selector = self.workspace_folder.is_some();
            let ws = crate::commands::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let target_desc = ws.display().to_string();
            super::exec::resolve_workspace_container_or_pick(
                client.as_ref(),
                &ws,
                None,
                None,
                has_explicit_selector,
                &target_desc,
                "Select a container:",
            )
            .await?
        };
        let container =
            super::resolve_service_container(client.as_ref(), container, self.service.as_deref())
                .await?;

        let cella_title =
            crate::title::title_for_container(&container, self.service.as_deref(), "shell");
        let title_guard =
            crate::title::push_for_container(&container, self.service.as_deref(), "shell");

        super::ensure_cella_daemon().await;

        // Read exec metadata from container labels.
        let user = container
            .labels
            .get("dev.cella.remote_user")
            .cloned()
            .or_else(|| container.container_user.clone())
            .unwrap_or_else(|| "root".to_string());
        let working_dir = container.labels.get("dev.cella.workspace_folder").cloned();

        let shell = resolve_shell(client.as_ref(), &container, &user, self.shell).await;
        debug!("Using shell: {}", shell.shell);

        let env = build_shell_env(client.as_ref(), &container, &user, &cella_title).await;

        let exit_code = client
            .as_ref()
            .exec_interactive(
                &container.id,
                &InteractiveExecOptions {
                    cmd: vec![shell.shell, "-l".to_string()],
                    user: Some(user),
                    env: Some(env),
                    working_dir,
                    tty: true,
                },
            )
            .await?;

        // process::exit skips Drop, so pop the title explicitly first.
        drop(title_guard);
        std::process::exit(i32::try_from(exit_code).unwrap_or(125));
    }
}

/// Build the shell environment: probed env merged with label env, then
/// `SSH_AUTH_SOCK`, AI keys, terminal vars, and `CELLA_TITLE` on top.
async fn build_shell_env(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    user: &str,
    cella_title: &str,
) -> Vec<String> {
    let label_env: Vec<String> = container
        .labels
        .get("dev.cella.remote_env")
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default();

    let probe_type = super::resolve_probe_type_from_labels(
        &container.labels,
        cella_env::user_env_probe::UserEnvProbe::default(),
    );
    let base_env = if let Some(probed) = cella_orchestrator::env_cache::read_probed_env_cache(
        client,
        &container.id,
        user,
        probe_type,
    )
    .await
    {
        cella_env::user_env_probe::merge_env(&probed, &label_env)
    } else {
        label_env
    };
    let mut env = base_env;

    cella_orchestrator::env_cache::ensure_ssh_auth_sock(client, &container.id, user, &mut env)
        .await;
    super::append_ai_keys(&mut env, &container.labels).await;

    for var in super::TERMINAL_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            env.push(format!("{var}={val}"));
        }
    }
    env.push(format!("CELLA_TITLE={cella_title}"));
    env
}

async fn resolve_shell(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    user: &str,
    cli_shell: Option<String>,
) -> ShellResolution {
    if let Some(s) = cli_shell {
        return ShellResolution {
            shell: s,
            source: ShellSource::CliFlag,
        };
    }

    let preferred = super::load_shell_preferred(&container.labels);
    let resolution =
        cella_orchestrator::shell_detect::resolve_shell(client, &container.id, user, &preferred)
            .await;

    if !preferred.is_empty()
        && !matches!(
            resolution.source,
            ShellSource::Preferred | ShellSource::CliFlag
        )
    {
        warn!(
            "Preferred shells not available, falling back to {}",
            resolution.shell,
        );
    }

    resolution
}
