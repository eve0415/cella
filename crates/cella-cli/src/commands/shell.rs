use std::path::PathBuf;

use clap::Args;
use tracing::{debug, warn};

use cella_docker::{ContainerTarget, DockerClient, ExecOptions, InteractiveExecOptions};

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

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,
}

impl ShellArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: None,
            workspace_folder: self.workspace_folder,
        };

        let container = target.resolve(&client, true).await?;

        super::ensure_credential_proxy();

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
            detect_shell(&client, &container.id, &user).await
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

        for var in TERMINAL_ENV_VARS {
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

/// Detect the best available shell in the container.
///
/// Tries, in order:
/// 1. `$SHELL` environment variable
/// 2. `/etc/passwd` entry for the user
/// 3. Probing `/bin/zsh`, `/bin/bash`, `/bin/sh`
async fn detect_shell(client: &DockerClient, container_id: &str, user: &str) -> String {
    // Try $SHELL
    if let Ok(result) = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo $SHELL".to_string(),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        let shell = result.stdout.trim().to_string();
        if !shell.is_empty() && shell != "$SHELL" {
            debug!("Detected shell from $SHELL: {shell}");
            return shell;
        }
    }

    // Try /etc/passwd
    if let Ok(result) = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("getent passwd {user} 2>/dev/null || grep '^{user}:' /etc/passwd"),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        let output = result.stdout.trim().to_string();
        if let Some(shell) = output.split(':').nth(6) {
            let shell = shell.trim();
            if !shell.is_empty() {
                debug!("Detected shell from passwd: {shell}");
                return shell.to_string();
            }
        }
    }

    // Probe common shells
    for candidate in &["/bin/zsh", "/bin/bash", "/bin/sh"] {
        if let Ok(result) = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "test".to_string(),
                        "-x".to_string(),
                        (*candidate).to_string(),
                    ],
                    user: Some(user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await
            && result.exit_code == 0
        {
            debug!("Detected shell by probing: {candidate}");
            return (*candidate).to_string();
        }
    }

    warn!("Could not detect shell, falling back to /bin/sh");
    "/bin/sh".to_string()
}

/// Terminal environment variables to forward into the container.
const TERMINAL_ENV_VARS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LANG",
    "COLUMNS",
    "LINES",
];
