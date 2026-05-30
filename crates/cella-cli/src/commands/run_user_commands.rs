//! `cella run-user-commands` — re-run lifecycle hooks against an existing
//! container.
//!
//! Mirrors the official devcontainer CLI `run-user-commands` command
//! (`runUserCommandsOptions` / `doRunUserCommands`, bound at
//! `devContainersSpecCLI.ts` line 74 — NOT `set-up`/`doSetUp`): it resolves an
//! already-running container, reads its devcontainer config (from `--config`
//! or the container's embedded metadata), runs the user lifecycle commands
//! against it via the official `runLifecycleHooks` gated-return order, and
//! emits `{"outcome":"success","result":"<status>"}` on success or
//! `{"outcome":"error","message":...,"description":...}` on failure (exit 1).
//!
//! Flag surface is `runUserCommandsOptions` verbatim. The data-folder fields,
//! `--docker-path`/`--docker-compose-path`, `--mount-*`,
//! `--skip-feature-auto-mapping`, and the terminal-size flags are accepted for
//! drop-in parity but are no-ops in cella (it manages its own data dirs, talks
//! to the engine API directly, resolves no features here, and has no PTY).
//! `--secrets-file` IS honored: its entries are injected into the lifecycle env.

use std::path::PathBuf;

use clap::Args;
use serde_json::{Value, json};

use cella_backend::ContainerTarget;
use cella_config::devcontainer::resolve;
use cella_env::user_env_probe::UserEnvProbe;
use cella_orchestrator::env_cache::probe_and_cache_user_env;
use cella_orchestrator::run_user_commands as orchestrator;
use cella_orchestrator::shell_detect::detect_shell;

use super::up::{map_env_object, parse_secrets_file};
use super::{LogFormat, LogLevel};
use crate::backend::BackendArgs;

/// Container-targeting flags. Mirrors the official `run-user-commands`
/// resolution surface: `--container-id` (highest priority), repeatable
/// `--id-label`, or `--workspace-folder` (defaults to cwd when none given).
#[derive(Args)]
pub struct TargetArgs {
    /// Id of the container to run the user commands for.
    #[arg(long = "container-id")]
    container_id: Option<String>,

    /// Id label(s) of the format `name=value` used to find the container
    /// (repeatable). If no `--container-id` is given the id labels are used to
    /// look up the container.
    #[arg(long = "id-label", value_parser = crate::commands::parse_id_label)]
    id_label: Vec<String>,

    /// Workspace folder path. The devcontainer.json is looked up relative to
    /// this path. Defaults to the current directory when no container target
    /// is given.
    #[arg(long = "workspace-folder")]
    workspace_folder: Option<PathBuf>,
}

/// Config-sourcing flags. `--config` layers an on-disk devcontainer.json on
/// top of the container's metadata; `--override-config` replaces it.
#[derive(Args)]
pub struct ConfigArgs {
    /// devcontainer.json path. When omitted, lifecycle commands are sourced
    /// from the container's embedded `devcontainer.metadata` label.
    #[arg(long)]
    config: Option<PathBuf>,

    /// devcontainer.json path to override any devcontainer.json in the
    /// workspace folder (or built-in configuration).
    #[arg(long = "override-config")]
    override_config: Option<PathBuf>,
}

/// Stop-after lifecycle-gating flags. Mirrors `runUserCommandsOptions`: there
/// is no `--skip-post-create` here (that belongs to `up`/`set-up`);
/// `postCreate` is always eligible. Split from [`AttachArgs`] to keep each
/// flattened sub-struct under the bool-count lint (clap flatten makes the
/// split invisible on the CLI surface).
#[derive(Args)]
pub struct GateArgs {
    /// Stop running user commands after the `waitFor` phase (default
    /// `updateContentCommand`).
    #[arg(long = "skip-non-blocking-commands")]
    skip_non_blocking_commands: bool,

    /// Stop after `onCreateCommand` and `updateContentCommand`.
    #[arg(long = "prebuild")]
    prebuild: bool,

    /// Stop for personalization (after dotfiles, before `postStartCommand`).
    #[arg(long = "stop-for-personalization")]
    stop_for_personalization: bool,
}

/// `postAttach`-related gating. Split from [`GateArgs`] purely to satisfy the
/// bool-count lint; flattened it merges back into the same CLI surface.
#[derive(Args)]
pub struct AttachArgs {
    /// Do not run `postAttachCommand`.
    #[arg(long = "skip-post-attach")]
    skip_post_attach: bool,
}

/// Compatibility/diagnostic flags accepted for devcontainer-CLI parity.
///
/// The data-folder fields, `--docker-path`/`--docker-compose-path`, and the
/// `--mount-*` flags are no-ops in cella (it manages its own data dirs and
/// talks to the engine API directly, not the `docker` CLI; mount layout is
/// fixed at create time). `--secrets-file` IS wired (read and injected into the
/// lifecycle env). `--terminal-columns`/`--terminal-rows` size lifecycle
/// subprocess output in the official CLI; cella's capture exec has no PTY, so
/// they are accepted-and-ignored (clap's `requires` enforces the pair).
#[derive(Args)]
pub struct CompatArgs {
    /// `docker` CLI binary path (compatibility no-op).
    #[arg(long = "docker-path")]
    docker_path: Option<String>,

    /// `docker compose` CLI binary path (compatibility no-op).
    #[arg(long = "docker-compose-path")]
    docker_compose_path: Option<String>,

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

    /// Mount the workspace using its Git root (compatibility no-op).
    #[arg(long = "mount-workspace-git-root", default_value_t = true)]
    mount_workspace_git_root: bool,

    /// Mount the Git worktree common dir (compatibility no-op).
    #[arg(long = "mount-git-worktree-common-dir")]
    mount_git_worktree_common_dir: bool,

    /// Temporary option for testing; cella resolves no features on this path
    /// (compatibility no-op, hidden — matches the official `hidden: true`).
    #[arg(long = "skip-feature-auto-mapping", hide = true)]
    skip_feature_auto_mapping: bool,

    /// Path to a JSON file of secret env vars, injected into the lifecycle env.
    #[arg(long = "secrets-file")]
    secrets_file: Option<PathBuf>,

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

    #[command(flatten)]
    config: ConfigArgs,

    /// Default value for the devcontainer.json's `userEnvProbe`.
    #[arg(
        long = "default-user-env-probe",
        value_enum,
        default_value_t = UserEnvProbe::LoginInteractiveShell,
    )]
    default_user_env_probe: UserEnvProbe,

    /// Remote environment variables of the format `name=value`, added when
    /// running the user (lifecycle) commands (repeatable).
    #[arg(long = "remote-env", value_parser = crate::commands::parse_remote_env)]
    remote_env: Vec<String>,

    #[command(flatten)]
    gate: GateArgs,

    #[command(flatten)]
    attach: AttachArgs,

    #[command(flatten)]
    dotfiles: crate::commands::DotfilesArgs,

    #[command(flatten)]
    pub(crate) compat: CompatArgs,
}

impl RunUserCommandsArgs {
    /// Resolve the container, read its config, re-run lifecycle hooks, and emit
    /// the result envelope. The envelope ALWAYS lands on stdout as JSON (there
    /// is no `--output` flag on this command); a failure prints the error
    /// envelope and exits 1, matching the official `run-user-commands`
    /// contract.
    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.run(progress).await {
            Ok(status) => {
                println!("{}", render_success_envelope(status));
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
    ) -> Result<&'static str, Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;
        let target = ContainerTarget {
            container_id: self.target.container_id.clone(),
            container_name: None,
            id_labels: self.target.id_label.clone(),
            workspace_folder: self.target.workspace_folder.clone(),
        };
        let container = target.resolve(&*client, false).await?;

        let workspace = container
            .labels
            .get("dev.cella.workspace_path")
            .map(PathBuf::from)
            .or_else(|| self.target.workspace_folder.clone());

        let config = self.read_config(workspace.as_deref())?;
        let metadata = container.labels.get("devcontainer.metadata").cloned();

        let remote_user = orchestrator::resolve_remote_user(&*client, &container, &config).await;
        let workspace_folder = workspace_folder_in_container(&config, &container);

        // Read --secrets-file up front so a bad file fails before any command
        // runs (mirrors official doRunUserCommands → readSecretsFromFile).
        let secrets = match self.compat.secrets_file.as_deref() {
            Some(path) => parse_secrets_file(path)?,
            None => Vec::new(),
        };

        let lifecycle_env = self
            .build_lifecycle_env(
                &*client,
                &container.id,
                &remote_user,
                &config,
                metadata.as_deref(),
                &secrets,
            )
            .await;

        let gating = self.build_gating(&config);
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
            gating,
            dotfiles: orchestrator::DotfilesInputs {
                repository: self.dotfiles.repository.as_deref(),
                install_command: self.dotfiles.install_command.as_deref(),
                target_path: &self.dotfiles.target_path,
            },
        };
        let status = orchestrator::run_user_commands(&lc_ctx, &input, &sender).await;
        drop(sender);
        let _ = renderer.await;
        status
    }

    /// Read the devcontainer config. With `--config`/`--override-config`,
    /// resolve it against the container's workspace; otherwise return an empty
    /// object so lifecycle is sourced entirely from the container's metadata
    /// label.
    fn read_config(
        &self,
        workspace: Option<&std::path::Path>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let Some(config_path) = self
            .config
            .override_config
            .as_deref()
            .or(self.config.config.as_deref())
        else {
            return Ok(json!({}));
        };
        let ws = workspace
            .map(std::path::Path::to_path_buf)
            .or_else(|| config_path.parent().map(std::path::Path::to_path_buf))
            .ok_or("could not determine workspace folder for --config")?;
        Ok(resolve::config(&ws, Some(config_path))?.config)
    }

    /// Probe the user environment and build the lifecycle env with the official
    /// precedence: probed < `--remote-env` < (config + metadata) `remoteEnv`.
    ///
    /// The official `probeRemoteEnv(updatedMergedConfig)` spreads the
    /// metadata-merged config's `remoteEnv`, not the raw on-disk one. cella's
    /// `parse_image_metadata` does not expose `remoteEnv`, so the merged
    /// `remoteEnv` is assembled here from (a) the metadata label's per-entry
    /// `remoteEnv` (later-wins, via `metadata_remote_env`) then (b) the `--config`
    /// `remoteEnv` layered last — the on-disk user config is the final metadata
    /// entry in the official merge, so it wins over earlier feature/base entries.
    ///
    /// `--secrets-file` entries are layered last of all, so they win over both
    /// probed and `remoteEnv` values, mirroring `up` and the official
    /// `runLifecycleHooks(..., secretsP)`.
    async fn build_lifecycle_env(
        &self,
        client: &dyn cella_backend::ContainerBackend,
        container_id: &str,
        remote_user: &str,
        config: &Value,
        metadata: Option<&str>,
        secrets: &[String],
    ) -> Vec<String> {
        // Order the vec for merge_env's later-wins insert: probed first
        // (provided by merge_env), then CLI --remote-env, then the merged
        // metadata+config remoteEnv last so it wins.
        let mut remote_env = self.remote_env.clone();
        remote_env.extend(orchestrator::metadata_remote_env(metadata));
        remote_env.extend(map_env_object(config.get("remoteEnv")));

        let probe_type = config
            .get("userEnvProbe")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(self.default_user_env_probe);
        let shell = detect_shell(client, container_id, remote_user).await;
        let probed =
            probe_and_cache_user_env(client, container_id, remote_user, probe_type, &shell).await;

        let mut lifecycle_env = probed.as_ref().map_or_else(
            || remote_env.clone(),
            |p| cella_env::user_env_probe::merge_env(p, &remote_env),
        );
        lifecycle_env.extend_from_slice(secrets);
        lifecycle_env
    }

    /// Build the lifecycle gating from the parity flags and resolved `waitFor`.
    fn build_gating(&self, config: &Value) -> orchestrator::Gating {
        orchestrator::Gating {
            stop: cella_backend::StopAfter {
                skip_non_blocking: self.gate.skip_non_blocking_commands,
                prebuild: self.gate.prebuild,
            },
            stop_for_personalization: self.gate.stop_for_personalization,
            skip_post_attach: self.attach.skip_post_attach,
            wait_for: cella_backend::WaitForPhase::from_config(config),
        }
    }
}

/// Render the JSON success envelope (single line). Matches official
/// `doRunUserCommands`: `{ outcome: 'success', result, dispose }` serializes to
/// `{"outcome":"success","result":"<status>"}` (`dispose` is a function, not
/// serialized); `result` is the `runLifecycleHooks` status string.
fn render_success_envelope(status: &str) -> String {
    serde_json::to_string(&json!({ "outcome": "success", "result": status })).unwrap_or_default()
}

/// Render the JSON error envelope (single line, no trailing newline).
///
/// Matches official `doRunUserCommands`'s non-`ContainerError` description.
fn render_error_envelope(message: &str) -> String {
    let output = json!({
        "outcome": "error",
        "message": message,
        "description": "An error occurred running user commands in the container.",
    });
    serde_json::to_string(&output).unwrap_or_default()
}

/// Resolve the in-container workspace folder.
///
/// Priority: config `workspaceFolder` > the container's
/// `dev.cella.workspace_folder` label (set at create time) > a
/// `/workspaces/<basename>` derived from the `dev.cella.workspace_path` host
/// label > `/workspaces`. The derivation mirrors cella's default mount target,
/// so the lifecycle `working_dir` stays correct even when neither `--config`
/// nor the folder label is present (the no-config, metadata-driven path).
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
    use super::*;
    use clap::CommandFactory;
    use std::collections::HashSet;

    // ── devcontainer-CLI flag parity ───────────────────────────────
    //
    // Source of truth: devcontainers/cli `src/spec-node/devContainersSpecCLI.ts`
    // `runUserCommandsOptions` (lines 786-815). Every official long flag MUST be
    // declared so no official invocation errors with "unknown argument".
    const OFFICIAL_RUN_USER_COMMANDS_FLAGS: &[&str] = &[
        "user-data-folder",
        "docker-path",
        "docker-compose-path",
        "container-data-folder",
        "container-system-data-folder",
        "workspace-folder",
        "mount-workspace-git-root",
        "mount-git-worktree-common-dir",
        "container-id",
        "id-label",
        "config",
        "override-config",
        "log-level",
        "log-format",
        "terminal-columns",
        "terminal-rows",
        "default-user-env-probe",
        "skip-non-blocking-commands",
        "prebuild",
        "stop-for-personalization",
        "remote-env",
        "skip-feature-auto-mapping",
        "skip-post-attach",
        "dotfiles-repository",
        "dotfiles-install-command",
        "dotfiles-target-path",
        "container-session-data-folder",
        "secrets-file",
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

        let missing: Vec<&&str> = OFFICIAL_RUN_USER_COMMANDS_FLAGS
            .iter()
            .filter(|f| !longs.contains(**f))
            .collect();
        assert!(
            missing.is_empty(),
            "`run-user-commands` is missing official flags: {missing:?}"
        );
    }

    #[test]
    fn no_target_flags_still_parses() {
        use clap::Parser;
        // Official defaults --workspace-folder to cwd when no target is given,
        // so an invocation with no targeting flag must parse (no required arg).
        let r = crate::Cli::try_parse_from(["cella", "run-user-commands"]);
        assert!(r.is_ok(), "no targeting flags should parse (cwd default)");
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
        assert!(r.is_ok(), "--id-label alone should parse");
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
    fn multiple_id_labels_are_all_retained() {
        use clap::Parser;
        // Regression: repeatable --id-label must keep EVERY value (official
        // AND-matches all of them). A prior bug truncated to the first label.
        let cli = crate::Cli::try_parse_from([
            "cella",
            "run-user-commands",
            "--id-label",
            "a=1",
            "--id-label",
            "b=2",
        ])
        .expect("two --id-label values must parse");
        let crate::commands::Command::RunUserCommands(args) = &cli.command else {
            panic!("expected run-user-commands subcommand");
        };
        assert_eq!(args.target.id_label, ["a=1", "b=2"]);
    }

    #[test]
    fn id_label_rejects_missing_value() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "run-user-commands", "--id-label", "novalue"]);
        assert!(r.is_err(), "--id-label without '=value' must be rejected");
    }

    #[test]
    fn skip_post_create_is_not_a_flag() {
        use clap::Parser;
        // --skip-post-create belongs to up/set-up, NOT run-user-commands
        // (postCreate is always eligible here); it must be rejected.
        let r = crate::Cli::try_parse_from([
            "cella",
            "run-user-commands",
            "--container-id",
            "abc",
            "--skip-post-create",
        ]);
        assert!(
            r.is_err(),
            "--skip-post-create must not exist on this command"
        );
    }

    #[test]
    fn include_configuration_is_not_a_flag() {
        use clap::Parser;
        // run-user-commands' envelope carries only outcome+result; the
        // configuration result flags belong to up/set-up.
        let r = crate::Cli::try_parse_from([
            "cella",
            "run-user-commands",
            "--container-id",
            "abc",
            "--include-configuration",
        ]);
        assert!(
            r.is_err(),
            "--include-configuration must not exist on this command"
        );
    }

    #[test]
    fn gating_flags_parse() {
        use clap::Parser;
        for flag in [
            "--skip-non-blocking-commands",
            "--prebuild",
            "--stop-for-personalization",
            "--skip-post-attach",
            "--skip-feature-auto-mapping",
        ] {
            let r = crate::Cli::try_parse_from([
                "cella",
                "run-user-commands",
                "--container-id",
                "abc",
                flag,
            ]);
            assert!(r.is_ok(), "{flag} should parse");
        }
    }

    #[test]
    fn success_envelope_shape() {
        let parsed: Value =
            serde_json::from_str(&render_success_envelope("done")).expect("valid JSON");
        assert_eq!(parsed["outcome"], "success");
        assert_eq!(parsed["result"], "done");
        // Must NOT carry the up/set-up container-centric keys.
        assert!(parsed.get("containerId").is_none());
        assert!(parsed.get("remoteUser").is_none());
        assert!(parsed.get("configuration").is_none());
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
}
