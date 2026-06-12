//! `cella set-up` — run lifecycle hooks and user personalisation inside an
//! already-running container.
//!
//! Mirrors the official devcontainer CLI `set-up` command (`setUpOptions` /
//! `doSetUp`, bound at `devContainersSpecCLI.ts`). Key differences from
//! `run-user-commands`:
//!
//! - `--container-id` is the ONLY container-targeting mechanism (required).
//!   No `--id-label`, no `--workspace-folder`.
//! - `--config` (optional): path to a devcontainer.json. When omitted, config
//!   is an empty object and lifecycle is sourced entirely from the container's
//!   `devcontainer.metadata` image label.
//! - `--skip-post-create`: skip all lifecycle hooks and dotfiles installation.
//! - `--include-configuration` / `--include-merged-configuration`: append the
//!   resolved and/or merged config to the JSON result envelope.
//!
//! The JSON result envelope on success is `{"outcome":"success"}` with
//! `"configuration"` and/or `"mergedConfiguration"` appended only when the
//! corresponding flags are passed. On failure: `{"outcome":"error","message":
//! "...","description":"An error occurred running user commands in the
//! container."}` then exit 1.

use std::path::PathBuf;

use clap::Args;
use serde_json::{Value, json};

use cella_backend::ContainerTarget;
use cella_env::user_env_probe::UserEnvProbe;
use cella_orchestrator::env_cache::probe_and_cache_user_env;
use cella_orchestrator::run_user_commands as orchestrator;
use cella_orchestrator::shell_detect::detect_shell;

use super::run_user_commands::{AttachArgs, CompatArgs};
use super::up::{map_env_object, parse_secrets_file};
use crate::backend::BackendArgs;

/// Gating flags for `set-up`. Subset of `run-user-commands` gating: official
/// `set-up` does not expose `--prebuild` or `--stop-for-personalization`.
#[derive(Args)]
struct SetUpGateArgs {
    /// Skip all lifecycle hooks and dotfiles. Corresponds to official
    /// `--skip-post-create`.
    #[arg(long = "skip-post-create")]
    skip_post_create: bool,

    /// Stop running user commands after the `waitFor` phase.
    #[arg(long = "skip-non-blocking-commands")]
    skip_non_blocking_commands: bool,
}

/// Result-shaping flags for `set-up`'s JSON output.
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

    /// Id of the container to set up.
    #[arg(long = "container-id", required = true)]
    container_id: String,

    /// Path to a devcontainer.json. When omitted, lifecycle commands are
    /// sourced from the container's embedded `devcontainer.metadata` label.
    #[arg(long = "config")]
    config: Option<PathBuf>,

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
    gate: SetUpGateArgs,

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
            container_id: Some(self.container_id.clone()),
            container_name: None,
            id_labels: Vec::new(),
            workspace_folder: None,
        };
        let container = target
            .resolve(&*client, false)
            .await
            .map_err(|_| "Dev container not found.")?;

        let config = self.read_config()?;
        let metadata = container.labels.get("devcontainer.metadata").cloned();

        // --container-id is the only resolution path for set-up, which matches
        // official's `hasIdLabels === false` branch: the on-disk --config is
        // appended to the baked metadata array.
        let effective_metadata = cella_backend::lifecycle::effective_lifecycle_metadata(
            metadata.as_deref(),
            &config,
            true,
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

        if !self.gate.skip_post_create {
            let gating = orchestrator::Gating {
                stop: cella_backend::StopAfter {
                    skip_non_blocking: self.gate.skip_non_blocking_commands,
                    prebuild: false,
                },
                stop_for_personalization: false,
                skip_post_attach: self.attach.skip_post_attach(),
                wait_for: cella_backend::WaitForPhase::from_metadata_or_config(
                    effective_metadata.as_deref(),
                    &config,
                ),
            };
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
            result?;
        }

        let mut envelope = json!({ "outcome": "success" });

        if self.result.include_configuration {
            envelope["configuration"] = config.clone();
        }
        if self.result.include_merged_configuration {
            let merged = effective_metadata
                .as_deref()
                .map_or_else(|| config.clone(), |meta| build_merged_config(meta, &config));
            envelope["mergedConfiguration"] = merged;
        }

        Ok(serde_json::to_string(&envelope).expect("BUG: success envelope is not serialisable"))
    }

    /// Read the devcontainer config. With `--config`, resolve and return it;
    /// if the path does not exist, error per official "Dev container config
    /// (<path>) not found." When omitted, return an empty object so lifecycle
    /// is sourced entirely from the container's `devcontainer.metadata` label.
    fn read_config(&self) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let Some(config_path) = self.config.as_deref() else {
            return Ok(json!({}));
        };
        if !config_path.exists() {
            return Err(format!(
                "Dev container config ({}) not found.",
                config_path.display()
            )
            .into());
        }
        let ws = config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or("could not determine workspace folder for --config")?;
        Ok(cella_config::devcontainer::resolve::config(&ws, Some(config_path))?.config)
    }

    /// Probe user env and build the lifecycle env with the same precedence as
    /// `run-user-commands`: probed < `--remote-env` < metadata `remoteEnv` <
    /// config `remoteEnv` < `--secrets-file`.
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
/// config, later-wins. Approximates `mergeConfiguration` for the
/// `--include-merged-configuration` envelope key without pulling in the full
/// feature-resolution pipeline.
fn build_merged_config(metadata_json: &str, config: &Value) -> Value {
    let entries: Vec<Value> = serde_json::from_str(metadata_json).unwrap_or_else(|e| {
        tracing::warn!("devcontainer.metadata label is not valid JSON, treating as empty: {e}");
        Vec::new()
    });
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
    serde_json::to_string(&json!({
        "outcome": "error",
        "message": message,
        "description": "An error occurred running user commands in the container.",
    }))
    .expect("BUG: error envelope is not serialisable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;
    use std::collections::HashSet;

    // Official setUpOptions flags (devContainersSpecCLI.ts) — the complete
    // list. `set-up` has no workspace/id-label targeting and no prebuild or
    // personalization gates.
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
    fn container_id_is_required() {
        use clap::Parser;
        let r = crate::Cli::try_parse_from(["cella", "set-up"]);
        assert!(r.is_err(), "set-up without --container-id must be rejected");
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
