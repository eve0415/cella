use std::path::{Path, PathBuf};

use clap::Args;
use tracing::warn;

use cella_backend::{ContainerTarget, ExecOptions, InteractiveExecOptions};
use cella_orchestrator::env_cache::{ensure_ssh_auth_sock, read_probed_env_cache};
use cella_orchestrator::shell_detect::{ShellSource, resolve_shell, wrap_in_login_shell};
use cella_orchestrator::tool_install::ToolName;

use crate::picker;
use crate::title::push_for_container;

/// Execute a command inside the running dev container.
#[derive(Args)]
pub struct ExecArgs {
    /// Default userEnvProbe type when the container has no probe label.
    #[arg(long, value_enum, default_value_t = cella_env::user_env_probe::UserEnvProbe::LoginInteractiveShell)]
    default_user_env_probe: cella_env::user_env_probe::UserEnvProbe,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Target container by ID.
    #[arg(long)]
    container_id: Option<String>,

    /// Target container by name.
    #[arg(long)]
    container_name: Option<String>,

    /// Target container by label(s) of the form `name=value` (repeatable).
    #[arg(long, value_parser = crate::commands::parse_id_label)]
    id_label: Vec<String>,

    /// Path to devcontainer.json. The default is .devcontainer/devcontainer.json
    /// or, if that does not exist, .devcontainer.json in the workspace folder.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to a devcontainer.json whose contents override any devcontainer.json
    /// in the workspace folder (required when there is none otherwise). The
    /// container is selected by the discovered config path, not this file's path.
    #[arg(long = "override-config")]
    override_config: Option<PathBuf>,

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
        let select_by_config = self.config_selects_container();
        let client = self.backend.resolve_client().await?;

        let user_opt = self.user;
        let workdir_opt = self.workdir;
        let remote_env = self.remote_env;
        let command = self.command;
        let service = self.service;
        let detach = self.detach;
        let output = self.output;
        let default_user_env_probe = self.default_user_env_probe;

        let container = if select_by_config {
            // Explicit config selection (no higher-precedence id target). Resolve
            // the path exactly as `up` does (config_with_override: `--config` wins;
            // with only `--override-config` the recorded path is the discovered
            // devcontainer.json, since the override supplies content, not the path),
            // then target the container by its spec identity labels
            // [local_folder + config_file] — the same (workspace, config) pair `up`
            // stamps and reuses by. Resolve the workspace folder with the same helper
            // `up` uses so `devcontainer.local_folder` matches byte-for-byte. Mirrors
            // the official `findContainerAndIdLabels` workspace tier; id targets
            // (handled below) take precedence. Explicit target → no picker.
            let ws = crate::commands::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let resolved = cella_config::devcontainer::resolve::config_with_override(
                &ws,
                self.config.as_deref(),
                self.override_config.as_deref(),
            )?;
            find_container_by_spec_identity(
                client.as_ref(),
                &resolved.workspace_root,
                &resolved.config_path,
            )
            .await?
        } else {
            let target = ContainerTarget {
                container_id: self.container_id,
                container_name: self.container_name,
                id_labels: self.id_label,
                workspace_folder: self.workspace_folder,
            };

            let has_explicit = picker::has_explicit_target(&target);
            match target.resolve(client.as_ref(), true).await {
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
            }
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
            default_user_env_probe,
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

    /// Whether `--config`/`--override-config` should drive container resolution.
    ///
    /// True only when a config path is given AND no higher-precedence explicit
    /// target (`--container-id`/`--container-name`/`--id-label`) is set. Mirrors
    /// the official `findContainerAndIdLabels` precedence, where the config file
    /// only feeds the workspace-tier label search and id targets win.
    const fn config_selects_container(&self) -> bool {
        (self.config.is_some() || self.override_config.is_some())
            && self.container_id.is_none()
            && self.container_name.is_none()
            && self.id_label.is_empty()
    }
}

/// The spec identity labels `up` stamps for a `(workspace, config)` pair:
/// `devcontainer.local_folder` and `devcontainer.config_file`, both lexical
/// (non-symlink-resolving) absolute paths. Byte-identical to what
/// `cella_backend::names::container_labels` (single-container) and
/// `build_compose_labels` (compose) write, so a container can be matched by them.
fn spec_identity_labels(workspace_root: &Path, config_path: &Path) -> [String; 2] {
    [
        format!(
            "devcontainer.local_folder={}",
            cella_backend::lexical_absolute(workspace_root).to_string_lossy()
        ),
        format!(
            "devcontainer.config_file={}",
            cella_backend::lexical_absolute(config_path).to_string_lossy()
        ),
    ]
}

/// Resolve the running container stamped with the given `(workspace, config)`
/// spec identity (`--config` / `--override-config`), matching both the
/// `devcontainer.local_folder` and `devcontainer.config_file` labels `up`
/// records — the same pair cella keys container reuse on. An explicit target,
/// so there is no interactive fallback.
async fn find_container_by_spec_identity(
    client: &dyn cella_backend::ContainerBackend,
    workspace_root: &Path,
    config_path: &Path,
) -> Result<cella_backend::ContainerInfo, Box<dyn std::error::Error + Send + Sync>> {
    let labels = spec_identity_labels(workspace_root, config_path);
    let info = client
        .find_container_by_labels(&labels)
        .await?
        .ok_or_else(|| {
            format!(
                "no dev container found for config '{}' in workspace '{}' — run `cella up` for it first",
                config_path.display(),
                workspace_root.display()
            )
        })?;
    if info.state != cella_backend::ContainerState::Running {
        return Err(format!(
            "container '{}' for config '{}' exists but is not running; run `cella up` to start it",
            info.name,
            config_path.display()
        )
        .into());
    }
    Ok(info)
}

async fn resolve_exec_context(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    user_opt: Option<String>,
    workdir_opt: Option<String>,
    remote_env: Vec<String>,
    command: &[String],
    default_user_env_probe: cella_env::user_env_probe::UserEnvProbe,
) -> Result<
    (String, Option<String>, Vec<String>, Vec<String>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let label_user = container.labels.get("dev.cella.remote_user").cloned();
    let label_workdir = container.labels.get("dev.cella.workspace_folder").cloned();
    // Config remoteEnv (wins over --remote-env) and infra-forwarded env (loses
    // to --remote-env) are stored in separate labels so exec can apply the
    // correct 4-way precedence: probed < fwd_env < CLI < config.
    let config_remote_env: Vec<String> = container
        .labels
        .get("dev.cella.remote_env")
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default();
    let fwd_env: Vec<String> = container
        .labels
        .get("dev.cella.fwd_env")
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

    let env = build_exec_env(
        client,
        container,
        &user,
        remote_env,
        fwd_env,
        config_remote_env,
        default_user_env_probe,
    )
    .await;

    let preferred = crate::commands::load_shell_preferred(&container.labels);
    let resolution = resolve_shell(client, &container.id, &user, &preferred).await;

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

    let cmd = wrap_in_login_shell(&resolution.shell, command);

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
            serde_json::to_string_pretty(&json_output).unwrap_or_default()
        );
        if result.exit_code != 0 {
            std::process::exit(i32::try_from(result.exit_code).unwrap_or(125));
        }
    } else {
        let title_guard = push_for_container(container, service, "exec");
        let mut env = env;
        env.push(format!(
            "CELLA_TITLE={}",
            crate::title::title_for_container(container, service, "exec")
        ));
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

/// Layer the exec environment by precedence (low → high):
///   probed < infra `fwd_env` < CLI `--remote-env` < devcontainer.json `remoteEnv`
///
/// Matches the official CLI's `probeRemoteEnv` order while keeping infra-
/// generated forwarding vars (proxy, credential helpers) below explicit CLI
/// flags. Config `remoteEnv` still wins on key collision (official behaviour).
fn layer_exec_remote_env<S: std::hash::BuildHasher>(
    probed: &std::collections::HashMap<String, String, S>,
    fwd_env: &[String],
    cli_remote_env: &[String],
    config_remote_env: &[String],
) -> Vec<String> {
    // Apply in ascending priority order so later entries overwrite earlier ones.
    let combined: Vec<String> = fwd_env
        .iter()
        .chain(cli_remote_env)
        .chain(config_remote_env)
        .cloned()
        .collect();
    cella_env::user_env_probe::merge_env(probed, &combined)
}

async fn build_exec_env(
    client: &dyn cella_backend::ContainerBackend,
    container: &cella_backend::ContainerInfo,
    user: &str,
    cli_remote_env: Vec<String>,
    fwd_env: Vec<String>,
    config_remote_env: Vec<String>,
    default_user_env_probe: cella_env::user_env_probe::UserEnvProbe,
) -> Vec<String> {
    let probe_type =
        super::resolve_probe_type_from_labels(&container.labels, default_user_env_probe);
    let probed = read_probed_env_cache(client, &container.id, user, probe_type)
        .await
        .unwrap_or_default();
    // cella infra vars (SSH_AUTH_SOCK, AI keys, terminal) are layered on top
    // below — they intentionally win over both remoteEnv sources.
    let mut env = layer_exec_remote_env(&probed, &fwd_env, &cli_remote_env, &config_remote_env);
    ensure_ssh_auth_sock(client, &container.id, user, &mut env).await;
    super::append_ai_keys(&mut env, &container.labels).await;
    for var in super::TERMINAL_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            env.push(format!("{var}={val}"));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn id_label_is_repeatable() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "cella",
            "exec",
            "--id-label",
            "a=1",
            "--id-label",
            "b=2",
            "--",
            "true",
        ])
        .expect("two --id-label values must parse");
        let crate::commands::Command::Exec(args) = &cli.command else {
            panic!("expected exec subcommand");
        };
        assert_eq!(args.id_label, ["a=1", "b=2"]);
    }

    #[test]
    fn id_label_rejects_missing_value() {
        use clap::Parser;
        let r =
            crate::Cli::try_parse_from(["cella", "exec", "--id-label", "novalue", "--", "true"]);
        assert!(r.is_err(), "--id-label without '=value' must be rejected");
    }

    #[test]
    fn config_and_override_config_parse() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "cella",
            "exec",
            "--config",
            "/a/devcontainer.json",
            "--override-config",
            "/b/override.json",
            "--",
            "true",
        ])
        .expect("--config/--override-config must parse");
        let crate::commands::Command::Exec(args) = &cli.command else {
            panic!("expected exec subcommand");
        };
        assert_eq!(
            args.config.as_deref(),
            Some(Path::new("/a/devcontainer.json"))
        );
        assert_eq!(
            args.override_config.as_deref(),
            Some(Path::new("/b/override.json"))
        );
    }

    #[test]
    fn spec_identity_labels_use_lexical_absolute_paths() {
        let labels = spec_identity_labels(
            Path::new("/repo"),
            Path::new("/repo/.devcontainer/devcontainer.json"),
        );
        assert_eq!(
            labels,
            [
                "devcontainer.local_folder=/repo".to_string(),
                "devcontainer.config_file=/repo/.devcontainer/devcontainer.json".to_string(),
            ]
        );
    }

    #[test]
    fn spec_identity_labels_make_relative_absolute() {
        let labels = spec_identity_labels(Path::new("ws"), Path::new("sub/devcontainer.json"));
        for (label, key) in labels
            .iter()
            .zip(["devcontainer.local_folder=", "devcontainer.config_file="])
        {
            let val = label.strip_prefix(key).expect("label key prefix");
            assert!(
                Path::new(val).is_absolute(),
                "expected absolute, got: {val}"
            );
        }
    }

    /// Parse an `exec` invocation and return its args for predicate inspection.
    fn exec_args(argv: &[&str]) -> ExecArgs {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(argv).expect("exec args must parse");
        let crate::commands::Command::Exec(args) = cli.command else {
            panic!("expected exec subcommand");
        };
        args
    }

    #[test]
    fn config_alone_selects_by_config() {
        let args = exec_args(["cella", "exec", "--config", "/a/dc.json", "--", "true"].as_ref());
        assert!(args.config_selects_container());
    }

    #[test]
    fn override_config_alone_selects_by_config() {
        let args = exec_args(
            [
                "cella",
                "exec",
                "--override-config",
                "/a/o.json",
                "--",
                "true",
            ]
            .as_ref(),
        );
        assert!(args.config_selects_container());
    }

    #[test]
    fn container_id_takes_precedence_over_config() {
        let args = exec_args(
            [
                "cella",
                "exec",
                "--config",
                "/a/dc.json",
                "--container-id",
                "abc",
                "--",
                "true",
            ]
            .as_ref(),
        );
        assert!(
            !args.config_selects_container(),
            "--container-id must win over --config (official precedence)"
        );
    }

    #[test]
    fn id_label_takes_precedence_over_config() {
        let args = exec_args(
            [
                "cella",
                "exec",
                "--config",
                "/a/dc.json",
                "--id-label",
                "k=v",
                "--",
                "true",
            ]
            .as_ref(),
        );
        assert!(
            !args.config_selects_container(),
            "--id-label must win over --config (official precedence)"
        );
    }

    #[test]
    fn container_name_takes_precedence_over_config() {
        let args = exec_args(
            [
                "cella",
                "exec",
                "--config",
                "/a/dc.json",
                "--container-name",
                "my-ctr",
                "--",
                "true",
            ]
            .as_ref(),
        );
        assert!(!args.config_selects_container());
    }

    #[test]
    fn no_config_does_not_select_by_config() {
        let args = exec_args(["cella", "exec", "--", "true"].as_ref());
        assert!(!args.config_selects_container());
    }

    #[test]
    fn config_remote_env_wins_over_cli_remote_env() {
        // Official precedence (probeRemoteEnv): probed < fwd < CLI --remote-env
        // < config remoteEnv. On a key collision, devcontainer.json wins.
        let probed = HashMap::from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("FOO".to_string(), "probed".to_string()),
        ]);
        let fwd = vec![];
        let cli = vec!["FOO=cli".to_string(), "ONLY_CLI=x".to_string()];
        let config = vec!["FOO=config".to_string()];

        let merged = layer_exec_remote_env(&probed, &fwd, &cli, &config);

        assert!(
            merged.contains(&"FOO=config".to_string()),
            "config must win"
        );
        assert!(!merged.contains(&"FOO=cli".to_string()));
        assert!(!merged.contains(&"FOO=probed".to_string()));
        // CLI-only and probed-only keys survive when not overridden.
        assert!(merged.contains(&"ONLY_CLI=x".to_string()));
        assert!(merged.contains(&"PATH=/usr/bin".to_string()));
    }

    #[test]
    fn cli_remote_env_wins_over_probed() {
        let probed = HashMap::from([("BAR".to_string(), "probed".to_string())]);
        let merged = layer_exec_remote_env(&probed, &[], &["BAR=cli".to_string()], &[]);
        assert!(merged.contains(&"BAR=cli".to_string()));
        assert!(!merged.contains(&"BAR=probed".to_string()));
    }

    #[test]
    fn cli_remote_env_wins_over_fwd_env() {
        // Regression: infra-forwarded env (proxy vars, SSH_AUTH_SOCK from
        // env_fwd) must NOT beat explicit --remote-env on a key collision.
        let probed = HashMap::new();
        let fwd = vec!["HTTPS_PROXY=http://infra-proxy:8080".to_string()];
        let cli = vec!["HTTPS_PROXY=http://user-proxy:3128".to_string()];
        let config = vec![];

        let merged = layer_exec_remote_env(&probed, &fwd, &cli, &config);

        assert!(
            merged.contains(&"HTTPS_PROXY=http://user-proxy:3128".to_string()),
            "CLI --remote-env must beat infra fwd_env"
        );
        assert!(!merged.contains(&"HTTPS_PROXY=http://infra-proxy:8080".to_string()));
    }
}
