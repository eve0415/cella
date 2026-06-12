//! `cella set-up` — run lifecycle hooks and user personalisation inside an
//! already-running container.
//!
//! Mirrors the official devcontainer CLI `set-up` command (`setUpOptions` /
//! `doSetUp`, bound at `devContainersSpecCLI.ts`). Unlike `run-user-commands`,
//! which re-runs every lifecycle phase unconditionally, `set-up` adds:
//!
//! - `--skip-post-create`: skip all lifecycle hooks (but still write the
//!   `etc/environment` and `etc/profile` patches and probe remote env).
//! - `--include-configuration` / `--include-merged-configuration`: append the
//!   resolved and/or merged config to the JSON result envelope (same surface as
//!   `up`).
//!
//! The JSON result envelope is the `up` envelope shape — `{outcome, result,
//! containerId, remoteUser, remoteWorkspaceFolder}` — NOT the lighter
//! `run-user-commands` shape (`{outcome, result}`). On failure the envelope
//! is `{outcome:"error", message, description}` and the process exits 1.
//!
//! Flag surface is `setUpOptions` verbatim. No-op compat flags (`--docker-path`,
//! `--container-data-folder`, etc.) are accepted for drop-in parity.

use std::path::PathBuf;

use clap::Args;
use serde_json::{Value, json};

use cella_backend::ContainerTarget;
use cella_env::user_env_probe::UserEnvProbe;
use cella_orchestrator::env_cache::probe_and_cache_user_env;
use cella_orchestrator::run_user_commands as orchestrator;
use cella_orchestrator::shell_detect::detect_shell;

use super::run_user_commands::{AttachArgs, CompatArgs, ConfigArgs, GateArgs, TargetArgs};
use super::up::{map_env_object, parse_secrets_file};
use crate::backend::BackendArgs;

/// Extra `set-up`-only flags. Split from `GateArgs` so that `--skip-post-create`
/// is absent on `run-user-commands` (parity test enforces this).
#[derive(Args)]
struct SetUpGateArgs {
    /// Skip all lifecycle hooks and dotfiles (still patches /etc/environment and
    /// probes remote env). Corresponds to official `--skip-post-create`.
    #[arg(long = "skip-post-create")]
    skip_post_create: bool,
}

/// Result-shaping flags for `set-up`'s JSON output. Same flags as `up`'s
/// `UpResultArgs`, but with `omit-config-remote-env-from-metadata` absent
/// (set-up doesn't build metadata — it reads it).
#[derive(Args)]
struct SetUpResultArgs {
    /// Include the substituted devcontainer.json in the JSON result.
    #[arg(long)]
    include_configuration: bool,

    /// Include the features-merged configuration in the JSON result.
    #[arg(long)]
    include_merged_configuration: bool,
}

/// Apply lifecycle hooks and user personalisation to an already-running container.
#[derive(Args)]
pub struct SetUpArgs {
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
    /// running the lifecycle commands (repeatable).
    #[arg(long = "remote-env", value_parser = crate::commands::parse_remote_env)]
    remote_env: Vec<String>,

    #[command(flatten)]
    gate: GateArgs,

    #[command(flatten)]
    set_up_gate: SetUpGateArgs,

    #[command(flatten)]
    attach: AttachArgs,

    #[command(flatten)]
    dotfiles: crate::commands::DotfilesArgs,

    #[command(flatten)]
    result: SetUpResultArgs,

    #[command(flatten)]
    pub(crate) compat: CompatArgs,
}

impl SetUpArgs {
    /// Resolve the container, read its metadata, run lifecycle hooks, and emit
    /// the result envelope. Always emits JSON to stdout; failures exit 1.
    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.run(progress).await {
            Ok(envelope) => {
                println!("{envelope}");
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
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;
        let target = ContainerTarget {
            container_id: self.target.container_id().cloned(),
            container_name: None,
            id_labels: self.target.id_labels().to_vec(),
            workspace_folder: self.target.workspace_folder().cloned(),
        };
        let container = target.resolve(&*client, false).await?;

        let workspace = container
            .labels
            .get("dev.cella.workspace_path")
            .map(PathBuf::from)
            .or_else(|| self.target.workspace_folder().cloned());

        let config = self.read_config(workspace.as_deref())?;
        let metadata = container.labels.get("devcontainer.metadata").cloned();

        // Same appends-config logic as run-user-commands: containers matched by
        // --container-id (not by label) get the on-disk --config appended.
        let appends_config = self.target.container_id().is_some();
        let effective_metadata = cella_backend::lifecycle::effective_lifecycle_metadata(
            metadata.as_deref(),
            &config,
            appends_config,
        );

        let remote_user = orchestrator::resolve_remote_user(&*client, &container, &config).await;
        let workspace_folder = workspace_folder_in_container(&config, &container);

        let secrets = match self.compat.secrets_file() {
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

        let status = if self.set_up_gate.skip_post_create {
            // Skip all hooks; still counts as done (matches official doSetUp
            // `skipPostCreate` path which skips runLifecycleHooks entirely).
            orchestrator::STATUS_DONE
        } else {
            let gating = self.build_gating(&config, effective_metadata.as_deref());
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
                metadata: effective_metadata.as_deref(),
                gating,
                dotfiles: orchestrator::DotfilesInputs {
                    repository: self.dotfiles.repository.as_deref(),
                    install_command: self.dotfiles.install_command.as_deref(),
                    target_path: &self.dotfiles.target_path,
                },
            };
            let result = orchestrator::run_user_commands(&lc_ctx, &input, &sender).await;
            drop(sender);
            let _ = renderer.await;
            result?
        };

        let mut envelope = json!({
            "outcome": "success",
            "result": status,
            "containerId": container.id,
            "remoteUser": remote_user,
            "remoteWorkspaceFolder": workspace_folder,
        });

        if self.result.include_configuration {
            envelope["configuration"] = config.clone();
        }
        if self.result.include_merged_configuration {
            if let Some(meta) = effective_metadata.as_deref() {
                // Build a minimal merged representation: spread metadata entries
                // into the config shape, giving the on-disk config the last word.
                let merged = build_merged_config(meta, &config);
                envelope["mergedConfiguration"] = merged;
            } else {
                envelope["mergedConfiguration"] = config.clone();
            }
        }

        Ok(serde_json::to_string(&envelope).unwrap_or_default())
    }

    /// Read the devcontainer config. With `--config`/`--override-config`,
    /// resolve it against the container's workspace; otherwise return an empty
    /// object so lifecycle is sourced entirely from the container's metadata label.
    fn read_config(
        &self,
        workspace: Option<&std::path::Path>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let Some(config_path) = self
            .config
            .override_config()
            .or_else(|| self.config.config())
        else {
            return Ok(json!({}));
        };
        let ws = workspace
            .map(std::path::Path::to_path_buf)
            .or_else(|| config_path.parent().map(std::path::Path::to_path_buf))
            .ok_or("could not determine workspace folder for --config")?;
        Ok(cella_config::devcontainer::resolve::config(&ws, Some(config_path))?.config)
    }

    /// Probe user env and build the lifecycle env with the same precedence as
    /// `run-user-commands` (probed < `--remote-env` < metadata `remoteEnv` <
    /// config `remoteEnv` < `--secrets-file`).
    async fn build_lifecycle_env(
        &self,
        client: &dyn cella_backend::ContainerBackend,
        container_id: &str,
        remote_user: &str,
        config: &Value,
        metadata: Option<&str>,
        secrets: &[String],
    ) -> Vec<String> {
        let mut remote_env = self.remote_env.clone();
        remote_env.extend(orchestrator::metadata_remote_env(metadata));
        remote_env.extend(map_env_object(config.get("remoteEnv")));

        let probe_type = config
            .get("userEnvProbe")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| orchestrator::metadata_user_env_probe(metadata))
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

    /// Build lifecycle gating. `set-up` exposes the same `--skip-non-blocking-commands`,
    /// `--prebuild`, `--stop-for-personalization`, `--skip-post-attach` surface as
    /// `run-user-commands`; `--skip-post-create` is handled before entering this path.
    fn build_gating(
        &self,
        config: &Value,
        effective_metadata: Option<&str>,
    ) -> orchestrator::Gating {
        orchestrator::Gating {
            stop: cella_backend::StopAfter {
                skip_non_blocking: self.gate.skip_non_blocking_commands(),
                prebuild: self.gate.prebuild(),
            },
            stop_for_personalization: self.gate.stop_for_personalization(),
            skip_post_attach: self.attach.skip_post_attach(),
            wait_for: cella_backend::WaitForPhase::from_metadata_or_config(
                effective_metadata,
                config,
            ),
        }
    }
}

/// Resolve the in-container workspace folder (same logic as `run-user-commands`).
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

/// Build a minimal merged config by folding metadata entries into the on-disk
/// config, later-wins. This approximates `mergeConfiguration` for the
/// `--include-merged-configuration` envelope key without pulling in the full
/// feature-resolution pipeline.
fn build_merged_config(metadata_json: &str, config: &Value) -> Value {
    let entries: Vec<Value> = serde_json::from_str(metadata_json).unwrap_or_default();
    let mut merged = serde_json::Map::new();
    for entry in &entries {
        if let Some(obj) = entry.as_object() {
            for (k, v) in obj {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    // On-disk config wins last (matches official: it is the final entry in the
    // metadata array after mergeConfiguration appends it).
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    Value::Object(merged)
}

/// Render the JSON error envelope.
fn render_error_envelope(message: &str) -> String {
    let output = json!({
        "outcome": "error",
        "message": message,
        "description": "An error occurred setting up the container.",
    });
    serde_json::to_string(&output).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;
    use std::collections::HashSet;

    // Official setUpOptions flags (devContainersSpecCLI.ts).
    const OFFICIAL_SET_UP_FLAGS: &[&str] = &[
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
        "skip-post-create",
        "dotfiles-repository",
        "dotfiles-install-command",
        "dotfiles-target-path",
        "container-session-data-folder",
        "secrets-file",
        "include-configuration",
        "include-merged-configuration",
    ];

    #[test]
    fn set_up_flag_parity() {
        let cli = crate::Cli::command();
        let cmd = cli
            .find_subcommand("set-up")
            .expect("`set-up` subcommand must exist");
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
            "`set-up` is missing official flags: {missing:?}"
        );
    }

    #[test]
    fn skip_post_create_parses() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from([
            "cella",
            "set-up",
            "--container-id",
            "abc",
            "--skip-post-create",
        ]);
        assert!(r.is_ok(), "--skip-post-create must parse on set-up");
    }

    #[test]
    fn include_configuration_parses() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from([
            "cella",
            "set-up",
            "--container-id",
            "abc",
            "--include-configuration",
        ]);
        assert!(r.is_ok(), "--include-configuration must parse on set-up");
    }

    #[test]
    fn no_target_still_parses() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "set-up"]);
        assert!(r.is_ok(), "no target flags should parse (cwd default)");
    }

    #[test]
    fn error_envelope_shape() {
        let parsed: Value =
            serde_json::from_str(&render_error_envelope("boom")).expect("valid JSON");
        assert_eq!(parsed["outcome"], "error");
        assert_eq!(parsed["message"], "boom");
        assert_eq!(
            parsed["description"],
            "An error occurred setting up the container."
        );
    }

    #[test]
    fn build_merged_config_later_wins() {
        let meta = json!([
            {"remoteUser": "from-feature", "customizations": {"vscode": {}}},
            {"remoteUser": "root"}
        ])
        .to_string();
        let config = json!({"remoteUser": "vscode", "workspaceFolder": "/app"});
        let merged = build_merged_config(&meta, &config);
        // On-disk config wins for remoteUser
        assert_eq!(merged["remoteUser"], "vscode");
        // Config-only key survives
        assert_eq!(merged["workspaceFolder"], "/app");
        // Metadata-only key survives
        assert!(merged.get("customizations").is_some());
    }

    #[test]
    fn workspace_folder_prefers_config() {
        let cfg = json!({"workspaceFolder": "/srv/app"});
        let labels = std::iter::once(("dev.cella.workspace_folder", "/workspaces/x"))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let c = cella_backend::ContainerInfo {
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
        };
        assert_eq!(workspace_folder_in_container(&cfg, &c), "/srv/app");
    }
}
