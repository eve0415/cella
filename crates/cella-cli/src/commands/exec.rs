use std::path::PathBuf;

use clap::Args;
use tracing::warn;

use cella_backend::{ContainerTarget, ExecOptions, InteractiveExecOptions};
use cella_orchestrator::env_cache::{ensure_ssh_auth_sock, read_probed_env_cache};
use cella_orchestrator::shell_detect::{detect_shell, wrap_in_login_shell};
use cella_orchestrator::tool_install::ToolName;

use crate::picker;
use crate::title::push_for_container;

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

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    service: Option<String>,

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

    /// Output format. JSON mode captures output instead of running interactively.
    #[arg(long, value_enum, default_value = "text")]
    output: super::OutputFormat,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// The command to execute.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

impl ExecArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;

        let user_opt = self.user;
        let workdir_opt = self.workdir;
        let remote_env = self.remote_env;
        let command = self.command;
        let service = self.service;
        let detach = self.detach;
        let output = self.output;

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: self.id_label,
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
            super::resolve_service_container(client.as_ref(), container, service.as_deref())
                .await?;

        super::ensure_cella_daemon().await;

        let (user, working_dir, env, cmd) = resolve_exec_context(
            client.as_ref(),
            &container,
            user_opt,
            workdir_opt,
            remote_env,
            &command,
        )
        .await?;

        run_exec(
            client.as_ref(),
            &container,
            &command,
            service.as_deref(),
            detach,
            &output,
            user,
            working_dir,
            env,
            cmd,
        )
        .await
    }
}

async fn resolve_exec_context(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    user_opt: Option<String>,
    workdir_opt: Option<String>,
    remote_env: Vec<String>,
    command: &[String],
) -> Result<
    (String, Option<String>, Vec<String>, Vec<String>),
    Box<dyn std::error::Error + Send + Sync>,
> {
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

    let user = user_opt
        .or(label_user)
        .or_else(|| container.container_user.clone())
        .unwrap_or_else(|| "root".to_string());

    let working_dir = workdir_opt.or(label_workdir);

    let mut env = build_exec_env(client, container, &user, label_env).await;
    env.extend(remote_env);

    let shell = detect_shell(client, &container.id, &user).await;
    let cmd = wrap_in_login_shell(&shell, command);

    Ok((user, working_dir, env, cmd))
}

#[expect(clippy::too_many_arguments)]
async fn run_exec(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    command: &[String],
    service: Option<&str>,
    detach: bool,
    output: &super::OutputFormat,
    user: String,
    working_dir: Option<String>,
    env: Vec<String>,
    cmd: Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if detach {
        let exec_id = client
            .exec_detached(
                &container.id,
                &ExecOptions {
                    cmd,
                    user: Some(user),
                    env: Some(env),
                    working_dir,
                },
            )
            .await?;
        println!("{exec_id}");
    } else if matches!(output, super::OutputFormat::Json) {
        let result = client
            .exec_command(
                &container.id,
                &ExecOptions {
                    cmd,
                    user: Some(user),
                    env: Some(env),
                    working_dir,
                },
            )
            .await?;
        let json_output = serde_json::json!({
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
        });
        println!(
            "{}",
            serde_json::to_string(&json_output).unwrap_or_default()
        );
        if result.exit_code != 0 {
            std::process::exit(i32::try_from(result.exit_code).unwrap_or(125));
        }
    } else {
        let title_guard = push_for_container(container, service, "exec");
        let exit_code = client
            .exec_interactive(
                &container.id,
                &InteractiveExecOptions {
                    cmd,
                    user: Some(user),
                    env: Some(env),
                    working_dir,
                    tty: std::io::IsTerminal::is_terminal(&std::io::stdin()),
                },
            )
            .await?;
        drop(title_guard);
        if exit_code == 127
            && let Some(binary) = command.first()
            && let Some(tool) = ToolName::from_binary_name(binary)
        {
            eprintln!(
                "{binary} is not installed. Run `cella install {}` to install it.",
                tool.config_name(),
            );
        }
        std::process::exit(i32::try_from(exit_code).unwrap_or(125));
    }

    Ok(())
}

async fn build_exec_env(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    user: &str,
    label_env: Vec<String>,
) -> Vec<String> {
    let base_env = if let Some(probed) = read_probed_env_cache(client, &container.id, user).await {
        cella_env::user_env_probe::merge_env(&probed, &label_env)
    } else {
        label_env
    };
    let mut env = base_env;
    ensure_ssh_auth_sock(client, &container.id, user, &mut env).await;
    super::append_ai_keys(&mut env, &container.labels);
    for var in super::TERMINAL_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            env.push(format!("{var}={val}"));
        }
    }
    env
}
