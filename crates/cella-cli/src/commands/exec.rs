use std::path::PathBuf;

use clap::Args;
use tracing::warn;

use cella_docker::{ContainerTarget, DockerClient, ExecOptions, InteractiveExecOptions};

/// Execute a command inside the running dev container.
#[derive(Args)]
pub struct ExecArgs {
    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Target container by ID.
    #[arg(long)]
    container_id: Option<String>,

    /// Target container by name.
    #[arg(long)]
    container_name: Option<String>,

    /// Target container by label.
    #[arg(long)]
    id_label: Option<String>,

    /// Working directory inside the container.
    #[arg(long)]
    workdir: Option<String>,

    /// User to execute the command as.
    #[arg(long)]
    user: Option<String>,

    /// Environment variables to set (KEY=VALUE).
    #[arg(long = "remote-env")]
    remote_env: Vec<String>,

    /// Run the command in detached mode.
    #[arg(short, long)]
    detach: bool,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// The command to execute.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

impl ExecArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: self.id_label,
            workspace_folder: self.workspace_folder,
        };

        let container = target.resolve(&client, true).await?;

        // Read exec metadata from container labels
        let label_user = container.labels.get("dev.cella.remote_user").cloned();
        let label_workdir = container.labels.get("dev.cella.workspace_folder").cloned();
        let label_env: Vec<String> = container
            .labels
            .get("dev.cella.remote_env")
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();

        if label_user.is_none() {
            warn!("No exec metadata labels found on container. Run `cella up` to set them.");
        }

        // Determine user: --user > label > "root"
        let user = self
            .user
            .or(label_user)
            .unwrap_or_else(|| "root".to_string());

        // Determine workdir: --workdir > label
        let working_dir = self.workdir.or(label_workdir);

        // Build environment: label env + --remote-env + terminal env forwarding
        let mut env = label_env;
        env.extend(self.remote_env);

        // Forward terminal environment variables
        for var in TERMINAL_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                env.push(format!("{var}={val}"));
            }
        }

        if self.detach {
            let exec_id = client
                .exec_detached(
                    &container.id,
                    &ExecOptions {
                        cmd: self.command,
                        user: Some(user),
                        env: Some(env),
                        working_dir,
                    },
                )
                .await?;
            println!("{exec_id}");
        } else {
            let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
            let exit_code = client
                .exec_interactive(
                    &container.id,
                    &InteractiveExecOptions {
                        cmd: self.command,
                        user: Some(user),
                        env: Some(env),
                        working_dir,
                        tty: is_tty,
                    },
                )
                .await?;
            std::process::exit(i32::try_from(exit_code).unwrap_or(125));
        }

        Ok(())
    }
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
