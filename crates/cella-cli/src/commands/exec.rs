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
        let client = self.backend.resolve_client().await?;

        // Determine container resolution path before consuming self's fields.
        let has_id = self.has_id_target();
        // Capture whether the user gave any explicit selector before consuming self.
        // An explicit selector (--config, --override-config, or --workspace-folder)
        // means the user asked for a specific environment; on a spec-identity miss
        // we must error rather than showing the picker. Bare invocations (no selector,
        // not even a workspace override) degrade to the picker instead.
        // This matches picker::has_explicit_target, which includes workspace_folder.
        let has_explicit_selector = self.config.is_some()
            || self.override_config.is_some()
            || self.workspace_folder.is_some();
        // Keep a display-friendly copy of the config path for the error message
        // before self is consumed into resolve_workspace_folder / config.as_deref().
        let config_display = self
            .config
            .as_deref()
            .or(self.override_config.as_deref())
            .map(|p| p.display().to_string());

        let container = if has_id {
            // Explicit id target wins: --container-id, --container-name, or --id-label.
            // Resolve directly; no picker (explicit means explicit). Mirrors the
            // official findContainerAndIdLabels precedence where providedIdLabels win.
            let target = ContainerTarget {
                container_id: self.container_id,
                container_name: self.container_name,
                id_labels: self.id_label,
                workspace_folder: self.workspace_folder,
            };
            target.resolve(client.as_ref(), true).await?
        } else {
            // No id target: resolve workspace + config, then use spec-identity lookup
            // with legacy fallback (mirrors the official workspace tier). --config /
            // --override-config are honoured; absent both, the default
            // devcontainer.json is discovered automatically.
            let ws = crate::commands::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let maybe_container = resolve_by_spec_identity_or_fallback(
                client.as_ref(),
                &ws,
                self.config.as_deref(),
                self.override_config.as_deref(),
            )
            .await?;
            if let Some(c) = maybe_container {
                c
            } else if has_explicit_selector {
                // An explicit selector was given but no running container matched.
                // Error instead of showing the picker — the user targeted a specific
                // environment and it isn't running yet.
                let target_desc = config_display
                    .as_deref()
                    .unwrap_or_else(|| ws.to_str().unwrap_or("?"));
                return Err(format!(
                    "no dev container found for '{target_desc}' — run `cella up` to start it"
                )
                .into());
            } else {
                // Bare invocation (no explicit selector): fall back to the
                // interactive picker so the user can choose from running containers.
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
        };

        let user_opt = self.user;
        let workdir_opt = self.workdir;
        let remote_env = self.remote_env;
        let command = self.command;
        let service = self.service;
        let detach = self.detach;
        let output = self.output;
        let default_user_env_probe = self.default_user_env_probe;

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

    /// Whether an explicit id target was given: any of `--container-id`,
    /// `--container-name`, or `--id-label`. These win over workspace/config
    /// resolution per the official `findContainerAndIdLabels` precedence.
    const fn has_id_target(&self) -> bool {
        self.container_id.is_some() || self.container_name.is_some() || !self.id_label.is_empty()
    }
}

/// The spec identity labels `up` stamps for a `(workspace, config)` pair:
/// `devcontainer.local_folder` and `devcontainer.config_file`, both lexical
/// (non-symlink-resolving) absolute paths. Byte-identical to what
/// `cella_backend::names::container_labels` (single-container) and
/// `build_compose_labels` (compose) write, so a container can be matched by them.
///
/// `pub(super)` (visible within the `commands` module) so `up` can reuse the
/// same label computation when performing a best-effort container re-find on
/// failure (for the JSON error envelope's `containerId` field).
pub(super) fn spec_identity_labels(workspace_root: &Path, config_path: &Path) -> [String; 2] {
    [
        format!(
            "{}={}",
            cella_backend::LOCAL_FOLDER_LABEL,
            cella_backend::lexical_absolute(workspace_root).to_string_lossy()
        ),
        format!(
            "{}={}",
            cella_backend::CONFIG_FILE_LABEL,
            cella_backend::lexical_absolute(config_path).to_string_lossy()
        ),
    ]
}

/// Resolve the running container matching the given `(workspace, config)` pair
/// using the official spec-identity lookup with legacy fallback, mirroring
/// `findContainerAndIdLabels` from the official devcontainer CLI
/// (src/spec-node/utils.ts, lines 682-726).
///
/// # Resolution order
///
/// 1. `[local_folder + config_file]` exact match — containers stamped by a
///    recent `cella up` will be found here.
/// 2. `[local_folder]` only — legacy fallback for containers created before the
///    spec-identity scheme. A container that **has** a `devcontainer.config_file`
///    label but didn't match step 1 belongs to a *different* config; it is
///    silently discarded (official behaviour). Only containers without that label
///    (old-format, pre-spec-identity) are accepted here.
///
/// cella already uses lexical-absolute paths consistently, so the official's
/// normalised-path variant (step 1b in the spec) is not needed — one exact
/// lookup per label-set is sufficient.
///
/// Returns `Ok(Some(container))` if found and Running, `Ok(None)` if not found
/// (caller may fall back to interactive picker), or `Err` if found but not
/// Running (explicit error; don't silently drop a stopped container).
pub(super) async fn find_container_spec_identity_with_fallback(
    client: &dyn cella_backend::ContainerBackend,
    workspace_root: &Path,
    config_path: &Path,
) -> Result<Option<cella_backend::ContainerInfo>, Box<dyn std::error::Error + Send + Sync>> {
    let new_labels = spec_identity_labels(workspace_root, config_path);

    // Step 1: exact [local_folder + config_file] match.
    if let Some(container) = client.find_container_by_labels(&new_labels).await? {
        return check_running(container, config_path).map(Some);
    }

    // Step 2: legacy fallback — [local_folder] only.
    // Discard the result if it carries a config_file label: it belongs to a
    // different config. Only old containers (created before the config_file label
    // was introduced) are accepted.
    let old_labels = [new_labels[0].clone()];
    if let Some(container) = client.find_container_by_labels(&old_labels).await? {
        if container
            .labels
            .contains_key(cella_backend::CONFIG_FILE_LABEL)
        {
            return Ok(None); // belongs to a different config; treat as not found
        }
        return check_running(container, config_path).map(Some);
    }

    Ok(None)
}

fn check_running(
    info: cella_backend::ContainerInfo,
    config_path: &Path,
) -> Result<cella_backend::ContainerInfo, Box<dyn std::error::Error + Send + Sync>> {
    if info.state == cella_backend::ContainerState::Running {
        Ok(info)
    } else {
        Err(format!(
            "container '{}' for config '{}' exists but is not running; run `cella up` to start it",
            info.name,
            config_path.display()
        )
        .into())
    }
}

/// Resolve the default (or explicit) devcontainer config for the workspace, then
/// look up the running container via spec-identity with legacy fallback.
///
/// When `config_path_override` or `override_config_file` is supplied, the
/// resolution is explicit and errors on miss. When neither is given the default
/// devcontainer.json is discovered; if there is none, the function degrades to a
/// workspace-folder-only lookup (`find_container`) — matching the pre-spec-identity
/// behaviour and avoiding hard errors where cella previously succeeded.
///
/// Note: the workspace-folder-only degradation path does NOT apply the
/// `config_file` discard check. The discard rule's purpose is to reject a
/// container that belongs to a *different* config; with no config resolved there
/// is nothing to disambiguate against, and applying the discard would break
/// containers that cella currently finds successfully.
///
/// `pub(super)` so `shell` and `install` can share this logic without duplicating
/// the spec-identity + legacy-fallback algorithm.
pub(super) async fn resolve_by_spec_identity_or_fallback(
    client: &dyn cella_backend::ContainerBackend,
    workspace_root: &Path,
    config_path_override: Option<&Path>,
    override_config_file: Option<&Path>,
) -> Result<Option<cella_backend::ContainerInfo>, Box<dyn std::error::Error + Send + Sync>> {
    if let Ok(resolved) = cella_config::devcontainer::resolve::config_with_override(
        workspace_root,
        config_path_override,
        override_config_file,
    ) {
        find_container_spec_identity_with_fallback(
            client,
            &resolved.workspace_root,
            &resolved.config_path,
        )
        .await
    } else {
        // No devcontainer.json found; degrade to workspace-folder-only lookup.
        // This preserves the behaviour callers had before spec-identity was added.
        let maybe = client.find_container(workspace_root).await?;
        Ok(maybe.and_then(|c| {
            if c.state == cella_backend::ContainerState::Running {
                Some(c)
            } else {
                None
            }
        }))
    }
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
    fn no_flags_has_no_id_target() {
        let args = exec_args(["cella", "exec", "--", "true"].as_ref());
        assert!(!args.has_id_target());
    }

    #[test]
    fn config_alone_has_no_id_target() {
        // --config drives workspace-tier resolution but is not an id target.
        let args = exec_args(["cella", "exec", "--config", "/a/dc.json", "--", "true"].as_ref());
        assert!(!args.has_id_target());
    }

    #[test]
    fn override_config_alone_has_no_id_target() {
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
        assert!(!args.has_id_target());
    }

    #[test]
    fn container_id_is_id_target() {
        let args = exec_args(["cella", "exec", "--container-id", "abc", "--", "true"].as_ref());
        assert!(
            args.has_id_target(),
            "--container-id must be detected as id target (official precedence)"
        );
    }

    #[test]
    fn id_label_is_id_target() {
        let args = exec_args(["cella", "exec", "--id-label", "k=v", "--", "true"].as_ref());
        assert!(
            args.has_id_target(),
            "--id-label must be detected as id target (official precedence)"
        );
    }

    #[test]
    fn container_name_is_id_target() {
        let args =
            exec_args(["cella", "exec", "--container-name", "my-ctr", "--", "true"].as_ref());
        assert!(args.has_id_target());
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

    // --- spec-identity resolution tests ---

    use cella_backend::error::BackendError;
    use cella_backend::traits::labels_match_all;
    use cella_backend::{
        BackendCapabilities, BackendKind, BoxFuture, BuildOptions, ContainerInfo, ContainerState,
        ExecOptions, ExecResult, FileToUpload, ImageDetails, InteractiveExecOptions, Platform,
    };

    /// Minimal mock backend that returns containers from a fixed list.
    ///
    /// `find_container_by_labels` filters the list using `labels_match_all`.
    /// `find_container` always returns `None` (not needed for spec-identity tests).
    struct MockBackend {
        containers: Vec<ContainerInfo>,
    }

    fn running_container(name: &str, labels: HashMap<String, String>) -> ContainerInfo {
        ContainerInfo {
            id: name.to_string(),
            name: name.to_string(),
            state: ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports: vec![],
            created_at: None,
            container_user: None,
            image: None,
            mounts: vec![],
            backend: BackendKind::Docker,
        }
    }

    impl cella_backend::ContainerBackend for MockBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                compose: false,
                managed_agent: false,
            }
        }

        fn find_container<'a>(
            &'a self,
            _: &'a Path,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            Box::pin(async { Ok(None) })
        }

        fn find_container_by_labels<'a>(
            &'a self,
            labels: &'a [String],
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            let result = self
                .containers
                .iter()
                .find(|c| labels_match_all(&c.labels, labels))
                .cloned();
            Box::pin(async move { Ok(result) })
        }

        fn create_container<'a>(
            &'a self,
            _: &'a cella_backend::CreateContainerOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn start_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn stop_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn remove_container<'a>(
            &'a self,
            _: &'a str,
            _: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn inspect_container<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
            unimplemented!()
        }

        fn list_cella_containers(
            &self,
            _: bool,
        ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn find_compose_service<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn find_container_by_label<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn container_logs<'a>(
            &'a self,
            _: &'a str,
            _: u32,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn exec_command<'a>(
            &'a self,
            _: &'a str,
            _: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            unimplemented!()
        }

        fn exec_stream<'a>(
            &'a self,
            _: &'a str,
            _: &'a ExecOptions,
            _: Box<dyn std::io::Write + Send + 'a>,
            _: Box<dyn std::io::Write + Send + 'a>,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            unimplemented!()
        }

        fn exec_interactive<'a>(
            &'a self,
            _: &'a str,
            _: &'a InteractiveExecOptions,
        ) -> BoxFuture<'a, Result<i64, BackendError>> {
            unimplemented!()
        }

        fn exec_detached<'a>(
            &'a self,
            _: &'a str,
            _: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn pull_image<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn build_image<'a>(
            &'a self,
            _: &'a BuildOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn image_exists<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
            unimplemented!()
        }

        fn tag_image<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn inspect_image_details<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
            unimplemented!()
        }

        fn upload_files<'a>(
            &'a self,
            _: &'a str,
            _: &'a [FileToUpload],
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            unimplemented!()
        }

        fn host_gateway(&self) -> &'static str {
            "host.docker.internal"
        }

        fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>> {
            unimplemented!()
        }

        fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
            unimplemented!()
        }

        fn inspect_image_env<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
            unimplemented!()
        }

        fn inspect_image_user<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            unimplemented!()
        }

        fn ensure_container_network<'a>(
            &'a self,
            _: &'a str,
            _: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn get_container_ip<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
            unimplemented!()
        }

        fn ensure_agent_provisioned<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn write_agent_addr<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn agent_volume_mount(&self) -> (String, String, bool) {
            unimplemented!()
        }

        fn prune_old_agent_versions<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }
    }

    fn ws() -> &'static Path {
        Path::new("/ws")
    }

    fn cfg() -> &'static Path {
        Path::new("/ws/.devcontainer/devcontainer.json")
    }

    fn spec_labels() -> HashMap<String, String> {
        let [lf, cf] = spec_identity_labels(ws(), cfg());
        let (lf_k, lf_v) = lf.split_once('=').unwrap();
        let (cf_k, cf_v) = cf.split_once('=').unwrap();
        HashMap::from([
            (lf_k.to_string(), lf_v.to_string()),
            (cf_k.to_string(), cf_v.to_string()),
        ])
    }

    fn local_folder_label_only() -> HashMap<String, String> {
        let [lf, _] = spec_identity_labels(ws(), cfg());
        let (k, v) = lf.split_once('=').unwrap();
        HashMap::from([(k.to_string(), v.to_string())])
    }

    fn spec_labels_different_config() -> HashMap<String, String> {
        // Same local_folder as ws() but a different config_file.
        let [lf, _] = spec_identity_labels(ws(), cfg());
        let (lf_k, lf_v) = lf.split_once('=').unwrap();
        HashMap::from([
            (lf_k.to_string(), lf_v.to_string()),
            (
                cella_backend::CONFIG_FILE_LABEL.to_string(),
                "/ws/.devcontainer/other.json".to_string(),
            ),
        ])
    }

    /// Container with `[local_folder + config_file]` → selected via exact match.
    #[tokio::test]
    async fn spec_identity_hit_selects_container() {
        let backend = MockBackend {
            containers: vec![running_container("target", spec_labels())],
        };
        let result = find_container_spec_identity_with_fallback(&backend, ws(), cfg())
            .await
            .expect("no backend error");
        assert!(result.is_some(), "should find the spec-identity container");
        assert_eq!(result.unwrap().name, "target");
    }

    /// Only a legacy container (`local_folder` only, no `config_file` label) → accepted.
    #[tokio::test]
    async fn legacy_fallback_hit_accepted() {
        let backend = MockBackend {
            containers: vec![running_container("legacy", local_folder_label_only())],
        };
        let result = find_container_spec_identity_with_fallback(&backend, ws(), cfg())
            .await
            .expect("no backend error");
        assert!(
            result.is_some(),
            "legacy container should be found via fallback"
        );
        assert_eq!(result.unwrap().name, "legacy");
    }

    /// A container with the right `local_folder` but a DIFFERENT `config_file` label
    /// is discarded in the legacy fallback — it belongs to another config.
    #[tokio::test]
    async fn legacy_fallback_discard_different_config_file() {
        let backend = MockBackend {
            containers: vec![running_container(
                "wrong-config",
                spec_labels_different_config(),
            )],
        };
        let result = find_container_spec_identity_with_fallback(&backend, ws(), cfg())
            .await
            .expect("no backend error");
        assert!(
            result.is_none(),
            "container with a different config_file label must be discarded in fallback"
        );
    }

    /// Two containers, same `local_folder`, different `config_file` → only the one
    /// matching our `config_file` is selected (exact match wins).
    ///
    /// `MockBackend::find_container_by_labels` scans all containers with
    /// `labels_match_all`, which mirrors the Docker production implementation
    /// (`docker_api_impl.rs`): it overrides the trait default to pass all labels
    /// to Docker's native multi-label AND-filter, so Docker returns only the
    /// matching container rather than the first container sharing `local_folder`.
    #[tokio::test]
    async fn multi_config_selects_matching_config() {
        let backend = MockBackend {
            containers: vec![
                running_container("other-config", spec_labels_different_config()),
                running_container("our-config", spec_labels()),
            ],
        };
        let result = find_container_spec_identity_with_fallback(&backend, ws(), cfg())
            .await
            .expect("no backend error");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().name,
            "our-config",
            "exact [local_folder + config_file] match must win over sibling configs"
        );
    }

    /// An id target (`--container-id`) causes `has_id_target()` to return true,
    /// so the spec-identity path is bypassed entirely — id targets always win.
    #[test]
    fn id_target_bypasses_spec_identity() {
        let args = exec_args(["cella", "exec", "--container-id", "abc123", "--", "true"].as_ref());
        assert!(
            args.has_id_target(),
            "--container-id must be recognised as id target so spec-identity is skipped"
        );
    }

    /// `--config` is an explicit config selector (not an id target).
    /// On a spec-identity miss, execution must error rather than show the picker.
    /// This test verifies the flag is recognised as an explicit config selector by
    /// checking neither branch of the id-target path is taken.
    #[test]
    fn config_flag_is_explicit_config_not_id_target() {
        let args = exec_args(["cella", "exec", "--config", "/a/dc.json", "--", "true"].as_ref());
        // Not an id target (picker bypass is separate from id-target bypass).
        assert!(!args.has_id_target());
        // config field set — execute() will set has_explicit_config = true and
        // return an error on miss instead of showing the picker.
        assert!(args.config.is_some());
    }

    /// `--override-config` is likewise an explicit config selector.
    #[test]
    fn override_config_flag_is_explicit_config_not_id_target() {
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
        assert!(!args.has_id_target());
        assert!(args.override_config.is_some());
    }
}
