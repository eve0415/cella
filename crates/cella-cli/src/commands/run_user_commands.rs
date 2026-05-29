//! `cella run-user-commands` — re-run lifecycle hooks against an existing
//! container.
//!
//! Mirrors the official devcontainer CLI `set-up` command (`setUpOptions` /
//! `doSetUp`): it resolves an already-running container, reads its devcontainer
//! config (from `--config` or the container's embedded metadata), and runs the
//! user lifecycle commands against it, gated by the same lifecycle flags as
//! `up`. The official `set-up` and `run-user-commands` share the same
//! lifecycle machinery; cella exposes it under the `run-user-commands` name.
//!
//! Flag surface is `setUpOptions` verbatim plus two cella-ergonomic additions
//! (`--id-label`, `--workspace-folder`) for resolving the target container when
//! `--container-id` is not the convenient handle.

use std::path::PathBuf;

use clap::Args;
use serde_json::{Value, json};

use cella_backend::ContainerTarget;
use cella_config::devcontainer::resolve;
use cella_env::user_env_probe::UserEnvProbe;
use cella_orchestrator::env_cache::probe_and_cache_user_env;
use cella_orchestrator::run_user_commands as orchestrator;
use cella_orchestrator::shell_detect::detect_shell;

use super::up::{UpResult, map_env_object, output_result, result_render_data};
use super::{LogFormat, LogLevel};
use crate::backend::BackendArgs;

/// Validate an `--id-label` value (`name=value`, both non-empty).
fn parse_id_label(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() && !v.is_empty() => Ok(s.to_string()),
        _ => Err("id-label must match <name>=<value>".to_string()),
    }
}

/// Validate a `--remote-env` value (`name=value`, value may be empty).
fn parse_remote_env(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, _)) if !k.is_empty() => Ok(s.to_string()),
        _ => Err("remote-env must match <name>=<value>".to_string()),
    }
}

/// Container-targeting flags. `--container-id` is the documented primary; the
/// repeatable `--id-label` and `--workspace-folder` are cella additions for
/// resolving the container when an id is inconvenient.
#[derive(Args)]
pub struct TargetArgs {
    /// Id of the container to run the user commands in.
    #[arg(
        long = "container-id",
        required_unless_present_any = ["id_label", "workspace_folder"],
    )]
    container_id: Option<String>,

    /// Id label(s) of the format `name=value` used to find the container
    /// (repeatable). cella addition; `--container-id` stays primary.
    #[arg(long = "id-label", value_parser = parse_id_label)]
    id_label: Vec<String>,

    /// Workspace folder whose container should be targeted. cella addition.
    #[arg(long = "workspace-folder")]
    workspace_folder: Option<PathBuf>,
}

/// Lifecycle-gating flags (subset of `setUpOptions` — no `--prebuild` or
/// `--skip-post-attach`, which belong to the sibling `run-user-commands`
/// handler in the official CLI; this command hardcodes both off).
#[derive(Args)]
pub struct GateArgs {
    /// Do not run onCreate/updateContent/postCreate/postStart/postAttach
    /// commands and do not install dotfiles.
    #[arg(long = "skip-post-create")]
    skip_post_create: bool,

    /// Stop running user commands after the `waitFor` phase (default
    /// updateContentCommand).
    #[arg(long = "skip-non-blocking-commands")]
    skip_non_blocking_commands: bool,
}

/// Dotfiles flags. Same surface as `up`.
#[derive(Args)]
pub struct DotfilesArgs {
    /// URL of a dotfiles Git repository to clone into the container.
    #[arg(long = "dotfiles-repository")]
    repository: Option<String>,

    /// Command to run after cloning the dotfiles repository. Defaults to the
    /// first of install.sh, install, bootstrap.sh, bootstrap, setup.sh, setup.
    #[arg(long = "dotfiles-install-command")]
    install_command: Option<String>,

    /// Path to clone the dotfiles repository to (default `~/dotfiles`).
    #[arg(long = "dotfiles-target-path", default_value = "~/dotfiles")]
    target_path: String,
}

/// Result-shaping flags. Same surface as `up`'s `--include-*`.
#[derive(Args)]
pub struct ResultArgs {
    /// Include the configuration in the JSON result.
    #[arg(long = "include-configuration")]
    include_configuration: bool,

    /// Include the merged configuration in the JSON result.
    #[arg(long = "include-merged-configuration")]
    include_merged_configuration: bool,
}

/// Compatibility/diagnostic flags accepted for devcontainer-CLI parity.
///
/// The data-folder fields and `--docker-path` are no-ops in cella (it manages
/// its own data dirs and talks to the engine API directly, not the `docker`
/// CLI). `--terminal-columns`/`--terminal-rows` size lifecycle subprocess
/// output in the official CLI; cella's capture exec has no PTY, so they are
/// accepted-and-ignored (clap's `requires` enforces the both-or-neither pair).
#[derive(Args)]
pub struct CompatArgs {
    /// `docker` CLI binary path (compatibility no-op).
    #[arg(long = "docker-path")]
    docker_path: Option<String>,

    /// Container data folder for in-container user data (compatibility no-op).
    #[arg(long = "container-data-folder")]
    container_data_folder: Option<PathBuf>,

    /// Container system data folder (compatibility no-op).
    #[arg(long = "container-system-data-folder")]
    container_system_data_folder: Option<PathBuf>,

    /// Per-session cache folder inside the container (compatibility no-op).
    #[arg(long = "container-session-data-folder")]
    container_session_data_folder: Option<PathBuf>,

    /// Host directory persisted across sessions (compatibility no-op).
    #[arg(long = "user-data-folder")]
    user_data_folder: Option<PathBuf>,

    /// Log verbosity for lifecycle/terminal logging.
    #[arg(long = "log-level", value_enum)]
    pub(crate) log_level: Option<LogLevel>,

    /// Log output format.
    #[arg(long = "log-format", value_enum, default_value = "text")]
    pub(crate) log_format: LogFormat,

    /// Number of columns to render subprocess output for (compatibility no-op).
    #[arg(long = "terminal-columns", requires = "terminal_rows")]
    terminal_columns: Option<u16>,

    /// Number of rows to render subprocess output for (compatibility no-op).
    #[arg(long = "terminal-rows", requires = "terminal_columns")]
    terminal_rows: Option<u16>,
}

/// Re-run the user (lifecycle) commands against an existing dev container.
#[derive(Args)]
pub struct RunUserCommandsArgs {
    #[command(flatten)]
    backend: BackendArgs,

    #[command(flatten)]
    target: TargetArgs,

    /// Path to devcontainer.json. When omitted, lifecycle commands are sourced
    /// from the container's embedded `devcontainer.metadata` label.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Default value for the devcontainer.json's `userEnvProbe`.
    #[arg(
        long = "default-user-env-probe",
        value_enum,
        default_value_t = UserEnvProbe::LoginInteractiveShell,
    )]
    default_user_env_probe: UserEnvProbe,

    /// Remote environment variables of the format `name=value`, added when
    /// running the user (lifecycle) commands (repeatable).
    #[arg(long = "remote-env", value_parser = parse_remote_env)]
    remote_env: Vec<String>,

    #[command(flatten)]
    gate: GateArgs,

    #[command(flatten)]
    dotfiles: DotfilesArgs,

    #[command(flatten)]
    result: ResultArgs,

    #[command(flatten)]
    pub(crate) compat: CompatArgs,
}

impl RunUserCommandsArgs {
    /// Resolve the container, read its config, re-run lifecycle hooks, and emit
    /// the result envelope. The envelope ALWAYS lands on stdout as JSON (there
    /// is no `--output` flag on this command); a failure prints the error
    /// envelope and exits 1, matching the official `set-up` contract.
    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.run(progress).await {
            Ok(result) => {
                output_result(&result_render_data(
                    &super::OutputFormat::Json,
                    &result_for_render(&result),
                ));
                Ok(())
            }
            Err(e) => {
                println!("{}", render_error_envelope(&e.to_string()));
                std::process::exit(1);
            }
        }
    }

    async fn run(
        self,
        progress: crate::progress::Progress,
    ) -> Result<RunResult, Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;
        let target = ContainerTarget {
            container_id: self.target.container_id.clone(),
            container_name: None,
            id_label: self.target.id_label.first().cloned(),
            workspace_folder: self.target.workspace_folder.clone(),
        };
        let container = target.resolve(&*client, false).await?;

        let workspace = container
            .labels
            .get("dev.cella.workspace_path")
            .map(PathBuf::from)
            .or_else(|| self.target.workspace_folder.clone());

        let (config, config_path) = self.read_config(workspace.as_deref())?;
        let metadata = container.labels.get("devcontainer.metadata").cloned();

        let remote_user = orchestrator::resolve_remote_user(&*client, &container, &config).await;
        let workspace_folder = workspace_folder_in_container(&config, &container);

        let lifecycle_env = self
            .build_lifecycle_env(&*client, &container.id, &remote_user, &config)
            .await;

        let gate = self.build_gate(&config);
        // Scope the borrows of `container.id` / `remote_user` /
        // `workspace_folder` (held by `lc_ctx`/`input`) to this block so they
        // can be moved into `RunResult` below.
        {
            let (sender, renderer) = crate::progress::bridge(&progress);
            let lc_ctx = cella_backend::LifecycleContext {
                client: &*client,
                container_id: &container.id,
                user: Some(&remote_user),
                env: &lifecycle_env,
                working_dir: Some(&workspace_folder),
                is_text: false,
                on_output: None,
            };
            let input = orchestrator::RunUserCommandsInput {
                config: &config,
                metadata: metadata.as_deref(),
                gate,
                dotfiles: orchestrator::DotfilesInputs {
                    repository: self.dotfiles.repository.as_deref(),
                    install_command: self.dotfiles.install_command.as_deref(),
                    target_path: &self.dotfiles.target_path,
                },
            };
            let run = orchestrator::run_user_commands(&lc_ctx, &input, &sender).await;
            drop(sender);
            let _ = renderer.await;
            run?;
        }

        let (configuration, merged_configuration) = self
            .build_envelope_extras(&config, config_path.as_deref())
            .await?;
        Ok(RunResult {
            container_id: container.id,
            remote_user,
            workspace_folder,
            configuration,
            merged_configuration,
        })
    }

    /// Read the devcontainer config. With `--config`, resolve it against the
    /// container's workspace; otherwise return an empty object so lifecycle is
    /// sourced entirely from the container's metadata label.
    fn read_config(
        &self,
        workspace: Option<&std::path::Path>,
    ) -> Result<(Value, Option<PathBuf>), Box<dyn std::error::Error + Send + Sync>> {
        let Some(config_path) = self.config.as_deref() else {
            return Ok((json!({}), None));
        };
        let ws = workspace
            .map(std::path::Path::to_path_buf)
            .or_else(|| config_path.parent().map(std::path::Path::to_path_buf))
            .ok_or("could not determine workspace folder for --config")?;
        let resolved = resolve::config(&ws, Some(config_path))?;
        Ok((resolved.config, Some(resolved.config_path)))
    }

    /// Probe the user environment and build the lifecycle env with the official
    /// precedence: probed < `--remote-env` < config `remoteEnv`.
    async fn build_lifecycle_env(
        &self,
        client: &dyn cella_backend::ContainerBackend,
        container_id: &str,
        remote_user: &str,
        config: &Value,
    ) -> Vec<String> {
        // config `remoteEnv` wins over CLI `--remote-env`, so order it last.
        let mut remote_env = self.remote_env.clone();
        remote_env.extend(map_env_object(config.get("remoteEnv")));

        let probe_type = config
            .get("userEnvProbe")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(self.default_user_env_probe);
        let shell = detect_shell(client, container_id, remote_user).await;
        let probed =
            probe_and_cache_user_env(client, container_id, remote_user, probe_type, &shell).await;

        probed.as_ref().map_or_else(
            || remote_env.clone(),
            |p| cella_env::user_env_probe::merge_env(p, &remote_env),
        )
    }

    /// Build the lifecycle gate. Per official `doSetUp`, `prebuild` and
    /// `skipPostAttach` are hardcoded off; only `--skip-post-create` and
    /// `--skip-non-blocking-commands` are honored.
    fn build_gate(&self, config: &Value) -> cella_backend::LifecycleGate {
        cella_backend::LifecycleGate::new(
            cella_backend::WaitForPhase::from_config(config),
            self.gate.skip_post_create,
            cella_backend::StopAfter {
                skip_non_blocking: self.gate.skip_non_blocking_commands,
                prebuild: false,
            },
            false,
        )
    }

    /// Build the optional `configuration` / `mergedConfiguration` envelope
    /// fields, honoring `--include-configuration` / `--include-merged-configuration`.
    async fn build_envelope_extras(
        &self,
        config: &Value,
        config_path: Option<&std::path::Path>,
    ) -> Result<(Option<Value>, Option<Value>), Box<dyn std::error::Error + Send + Sync>> {
        let configuration = if self.result.include_configuration {
            let mut cfg = config.clone();
            if let Some(path) = config_path {
                inject_config_file_path(&mut cfg, path);
            }
            Some(cfg)
        } else {
            None
        };

        let merged_configuration = if self.result.include_merged_configuration {
            let path = config_path.map_or_else(|| PathBuf::from("devcontainer.json"), Into::into);
            Some(super::read_configuration::resolve_merged_config(config, &path).await?)
        } else {
            None
        };

        Ok((configuration, merged_configuration))
    }
}

/// Container-side result fields needed for the success envelope.
struct RunResult {
    container_id: String,
    remote_user: String,
    workspace_folder: String,
    configuration: Option<Value>,
    merged_configuration: Option<Value>,
}

/// Borrow a [`RunResult`] as an [`UpResult`] so the shared `up` renderer emits
/// the official envelope keys (`containerId`, `remoteUser`,
/// `remoteWorkspaceFolder`, and the optional config objects).
fn result_for_render(result: &RunResult) -> UpResult {
    UpResult {
        container_id: result.container_id.clone(),
        remote_user: result.remote_user.clone(),
        // No granular provisioning state on this path; the container already
        // existed, so report it as running.
        outcome: "running".to_string(),
        workspace_folder: result.workspace_folder.clone(),
        ssh_agent_proxy: None,
        compose_project_name: None,
        configuration: result.configuration.clone(),
        merged_configuration: result.merged_configuration.clone(),
    }
}

/// Render the JSON error envelope (single line, no trailing newline).
///
/// Uses the official `set-up` failure description, which differs from `up`'s
/// (`render_error_result` in the `up` module hardcodes the container-setup
/// description), so this command keeps its own renderer.
fn render_error_envelope(message: &str) -> String {
    let output = json!({
        "outcome": "error",
        "message": message,
        "description": "An error occurred running user commands in the container.",
    });
    serde_json::to_string(&output).unwrap_or_default()
}

/// Inject a `configFilePath` URI object into a cloned `configuration`, matching
/// the shape the official CLI embeds.
fn inject_config_file_path(config: &mut Value, config_path: &std::path::Path) {
    let canonical = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let path_str = canonical.to_string_lossy();
    if let Some(obj) = config.as_object_mut() {
        obj.insert(
            "configFilePath".to_string(),
            json!({
                "fsPath": path_str,
                "$mid": 1,
                "path": path_str,
                "scheme": "file",
            }),
        );
    }
}

/// Resolve the in-container workspace folder.
///
/// Priority: config `workspaceFolder` > the container's
/// `dev.cella.workspace_folder` label (set at create time) > a
/// `/workspaces/<basename>` derived from the `dev.cella.workspace_path` host
/// label > `/workspaces`. The derivation mirrors cella's default mount target,
/// so the lifecycle `working_dir` and the `remoteWorkspaceFolder` envelope
/// value stay correct even when neither `--config` nor the folder label is
/// present (the no-config, metadata-driven path).
fn workspace_folder_in_container(
    config: &Value,
    container: &cella_backend::ContainerInfo,
) -> String {
    if let Some(f) = config.get("workspaceFolder").and_then(Value::as_str) {
        return f.to_string();
    }
    if let Some(f) = container.labels.get("dev.cella.workspace_folder") {
        return f.clone();
    }
    if let Some(basename) = container
        .labels
        .get("dev.cella.workspace_path")
        .map(std::path::Path::new)
        .and_then(std::path::Path::file_name)
    {
        return format!("/workspaces/{}", basename.to_string_lossy());
    }
    "/workspaces".to_string()
}

#[cfg(test)]
mod tests {
    use super::super::up::UpRenderData;
    use super::*;
    use clap::CommandFactory;
    use std::collections::HashSet;

    // ── devcontainer-CLI flag parity ───────────────────────────────
    //
    // Source of truth: devcontainers/cli `src/spec-node/devContainersSpecCLI.ts`
    // `setUpOptions`. Every official long flag MUST be declared so no official
    // invocation errors with "unknown argument". `--id-label` and
    // `--workspace-folder` are cella additions (not in this list).
    const OFFICIAL_SET_UP_FLAGS: &[&str] = &[
        "docker-path",
        "container-data-folder",
        "container-system-data-folder",
        "container-id",
        "config",
        "log-level",
        "log-format",
        "terminal-columns",
        "terminal-rows",
        "default-user-env-probe",
        "skip-post-create",
        "skip-non-blocking-commands",
        "user-data-folder",
        "remote-env",
        "dotfiles-repository",
        "dotfiles-install-command",
        "dotfiles-target-path",
        "container-session-data-folder",
        "include-configuration",
        "include-merged-configuration",
    ];

    #[test]
    fn run_user_commands_flag_parity() {
        let cli = crate::Cli::command();
        let cmd = cli
            .find_subcommand("run-user-commands")
            .expect("`run-user-commands` subcommand must exist");
        let longs: HashSet<&str> = cmd
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect();

        let missing: Vec<&&str> = OFFICIAL_SET_UP_FLAGS
            .iter()
            .filter(|f| !longs.contains(**f))
            .collect();
        assert!(
            missing.is_empty(),
            "`run-user-commands` is missing official set-up flags: {missing:?}"
        );
    }

    #[test]
    fn container_id_required_without_other_targets() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "run-user-commands"]);
        assert!(
            r.is_err(),
            "must require a container target when none is given"
        );
    }

    #[test]
    fn container_id_alone_parses() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "run-user-commands", "--container-id", "abc"]);
        assert!(r.is_ok(), "--container-id alone should parse");
    }

    #[test]
    fn id_label_alone_parses() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "run-user-commands", "--id-label", "foo=bar"]);
        assert!(
            r.is_ok(),
            "--id-label alone should satisfy the target requirement"
        );
    }

    #[test]
    fn remote_env_rejects_missing_equals() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from([
            "cella",
            "run-user-commands",
            "--container-id",
            "abc",
            "--remote-env",
            "NOEQUALS",
        ]);
        assert!(r.is_err(), "--remote-env without '=' must be rejected");
    }

    #[test]
    fn remote_env_accepts_empty_value() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from([
            "cella",
            "run-user-commands",
            "--container-id",
            "abc",
            "--remote-env",
            "EMPTY=",
        ]);
        assert!(r.is_ok(), "--remote-env with empty value must parse");
    }

    #[test]
    fn id_label_rejects_missing_value() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "run-user-commands", "--id-label", "novalue"]);
        assert!(r.is_err(), "--id-label without '=value' must be rejected");
    }

    #[test]
    fn skip_post_attach_is_not_a_flag() {
        use clap::Parser;
        // --skip-post-attach belongs to the sibling official handler, not
        // set-up; it must be rejected as an unknown argument here.
        let r = crate::Cli::try_parse_from([
            "cella",
            "run-user-commands",
            "--container-id",
            "abc",
            "--skip-post-attach",
        ]);
        assert!(
            r.is_err(),
            "--skip-post-attach must not exist on this command"
        );
    }

    #[test]
    fn error_envelope_shape() {
        let parsed: Value =
            serde_json::from_str(&render_error_envelope("boom")).expect("valid JSON");
        assert_eq!(parsed["outcome"], "error");
        assert_eq!(parsed["message"], "boom");
        assert_eq!(
            parsed["description"],
            "An error occurred running user commands in the container."
        );
    }

    fn container_with_labels(pairs: &[(&str, &str)]) -> cella_backend::ContainerInfo {
        let labels = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        cella_backend::ContainerInfo {
            id: "test".to_string(),
            name: "test".to_string(),
            state: cella_backend::ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports: Vec::new(),
            created_at: None,
            container_user: None,
            image: None,
            mounts: Vec::new(),
            backend: cella_backend::BackendKind::Docker,
        }
    }

    #[test]
    fn workspace_folder_prefers_config() {
        let cfg = json!({"workspaceFolder": "/srv/app"});
        let c = container_with_labels(&[("dev.cella.workspace_folder", "/workspaces/x")]);
        assert_eq!(workspace_folder_in_container(&cfg, &c), "/srv/app");
    }

    #[test]
    fn workspace_folder_uses_label_when_no_config() {
        let c = container_with_labels(&[("dev.cella.workspace_folder", "/workspaces/proj")]);
        assert_eq!(
            workspace_folder_in_container(&json!({}), &c),
            "/workspaces/proj"
        );
    }

    #[test]
    fn workspace_folder_derives_from_workspace_path() {
        let c = container_with_labels(&[("dev.cella.workspace_path", "/home/me/cool-repo")]);
        assert_eq!(
            workspace_folder_in_container(&json!({}), &c),
            "/workspaces/cool-repo"
        );
    }

    #[test]
    fn render_data_carries_official_keys() {
        let result = RunResult {
            container_id: "deadbeef".to_string(),
            remote_user: "vscode".to_string(),
            workspace_folder: "/workspaces/app".to_string(),
            configuration: None,
            merged_configuration: None,
        };
        let up = result_for_render(&result);
        let data: UpRenderData<'_> = result_render_data(&super::super::OutputFormat::Json, &up);
        let rendered = super::super::up::render_up_result(&data);
        let parsed: Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(parsed["outcome"], "success");
        assert_eq!(parsed["containerId"], "deadbeef");
        assert_eq!(parsed["remoteUser"], "vscode");
        assert_eq!(parsed["remoteWorkspaceFolder"], "/workspaces/app");
    }
}
