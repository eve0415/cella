use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use cella_backend::ContainerTarget;
use cella_config::devcontainer::resolve;
use cella_features::types::FeatureContainerConfig;

use crate::backend::BackendArgs;

/// Read and output the resolved devcontainer configuration.
#[derive(Args)]
pub struct ReadConfigurationArgs {
    #[command(flatten)]
    backend: BackendArgs,

    /// Workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Include merged configuration from features.
    #[arg(long)]
    include_merged_configuration: bool,

    /// Include features configuration details.
    #[arg(long)]
    include_features_configuration: bool,

    /// Path to a devcontainer.json that replaces the discovered config entirely.
    #[arg(long = "override-config")]
    override_config: Option<PathBuf>,

    /// Target container by label(s) of the form `name=value` (repeatable).
    #[arg(long = "id-label", value_parser = crate::commands::parse_id_label)]
    id_label: Vec<String>,

    /// Target container by ID.
    #[arg(long = "container-id")]
    container_id: Option<String>,

    /// Additional features as JSON string.
    #[arg(long = "additional-features")]
    additional_features: Option<String>,
}

impl ReadConfigurationArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let has_container_target = !self.id_label.is_empty() || self.container_id.is_some();

        let config_path = self.override_config.as_deref().or(self.config.as_deref());

        let (mut resolved, cwd) = if has_container_target {
            let client = self.backend.resolve_client().await?;
            let target = ContainerTarget {
                container_id: self.container_id.clone(),
                container_name: None,
                id_labels: self.id_label.clone(),
                workspace_folder: self.workspace_folder.clone(),
            };
            let container = target.resolve(&*client, false).await?;

            let workspace_path = container
                .labels
                .get("dev.cella.workspace_path")
                .ok_or("container has no dev.cella.workspace_path label")?;
            let ws = PathBuf::from(workspace_path);
            let res = resolve::config(&ws, config_path)?;
            (res, ws)
        } else {
            let ws = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let res = resolve::config(&ws, config_path)?;
            (res, ws)
        };

        if let Some(ref additional) = self.additional_features {
            super::features::resolve::merge_additional_features(&mut resolved.config, additional)?;
        }

        let device_type = if resolved.config.get("dockerComposeFile").is_some() {
            "dockerCompose"
        } else if resolved.config.get("build").is_some()
            || resolved.config.get("dockerFile").is_some()
        {
            "dockerfile"
        } else {
            "image"
        };

        let workspace_basename = cwd.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let workspace_folder = resolved
            .config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map_or_else(|| format!("/workspaces/{workspace_basename}"), String::from);

        let config_path = resolved
            .config_path
            .canonicalize()
            .unwrap_or_else(|_| resolved.config_path.clone());
        let config_path_str = config_path.to_string_lossy();

        let mut output = json!({
            "configFilePath": {
                "fsPath": config_path_str,
                "$mid": 1,
                "path": config_path_str,
                "scheme": "file"
            },
            "configuration": resolved.config,
            "workspace": {
                "workspaceFolder": workspace_folder,
                "deviceContainerType": device_type
            }
        });

        if self.include_merged_configuration {
            output["mergedConfiguration"] =
                resolve_merged_config(&resolved.config, &resolved.config_path).await?;
        }

        println!("{}", serde_json::to_string_pretty(&output)?);
        Ok(())
    }
}

/// Build the features-merged configuration for a resolved devcontainer.
///
/// Reused by `up --include-merged-configuration`.
///
/// KNOWN GAP: this performs an additive overwrite that keeps lifecycle keys
/// SINGULAR (`onCreateCommand`), whereas the official `mergeConfiguration`
/// emits PLURAL arrays (`onCreateCommands` etc.), a `customizations` Record,
/// and boolean-OR/union/last-wins scalar resolution. Tracked for a follow-up.
pub async fn resolve_merged_config(
    config: &serde_json::Value,
    config_path: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let features = super::features::resolve::extract_features(config);
    if features.is_empty() {
        return Ok(config.clone());
    }

    let platform =
        cella_features::oci::detect_platform(std::env::consts::OS, std::env::consts::ARCH);
    let cache = cella_features::cache::FeatureCache::new();
    let base_image = config.get("image").and_then(|v| v.as_str()).unwrap_or("");
    let base_ctx = cella_features::BaseImageContext {
        base_image,
        image_user: "root",
        metadata: None,
        // read-configuration only merges config; no label is persisted, so the
        // omit-remote-env flag does not apply here.
        omit_remote_env: false,
    };
    let rf = cella_features::resolve_features(
        config,
        config_path,
        &platform,
        &cache,
        &base_ctx,
        false,
        cella_features::LockfilePolicy::NoLockfile,
    )
    .await?;

    let mut merged = config.clone();
    apply_feature_config(&mut merged, &rf.container_config);
    Ok(merged)
}

fn apply_feature_config(config: &mut serde_json::Value, fc: &FeatureContainerConfig) {
    if !fc.mounts.is_empty() {
        config["mounts"] = json!(fc.mounts);
    }
    if !fc.cap_add.is_empty() {
        config["capAdd"] = json!(fc.cap_add);
    }
    if !fc.security_opt.is_empty() {
        config["securityOpt"] = json!(fc.security_opt);
    }
    if fc.privileged {
        config["privileged"] = json!(true);
    }
    if fc.init {
        config["init"] = json!(true);
    }
    if !fc.container_env.is_empty() {
        config["containerEnv"] = json!(fc.container_env);
    }
    if !fc.customizations.is_null() {
        config["customizations"] = fc.customizations.clone();
    }
    apply_lifecycle_field(config, "onCreateCommand", &fc.lifecycle.on_create);
    apply_lifecycle_field(config, "updateContentCommand", &fc.lifecycle.update_content);
    apply_lifecycle_field(config, "postCreateCommand", &fc.lifecycle.post_create);
    apply_lifecycle_field(config, "postStartCommand", &fc.lifecycle.post_start);
    apply_lifecycle_field(config, "postAttachCommand", &fc.lifecycle.post_attach);
}

fn apply_lifecycle_field(
    config: &mut serde_json::Value,
    key: &str,
    entries: &[cella_features::types::LifecycleEntry],
) {
    match entries.len() {
        0 => {}
        1 => config[key] = entries[0].command.clone(),
        _ => {
            let obj: serde_json::Map<String, serde_json::Value> = entries
                .iter()
                .map(|e| (e.origin.clone(), e.command.clone()))
                .collect();
            config[key] = serde_json::Value::Object(obj);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cella_features::types::{FeatureContainerConfig, FeatureLifecycle, LifecycleEntry};
    use serde_json::json;

    use super::*;

    #[test]
    fn id_label_is_repeatable_and_validated() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ReadConfigurationArgs,
        }

        // Repeatable: two --id-label flags collect into the Vec.
        let cli = Cli::try_parse_from([
            "read-configuration",
            "--id-label",
            "a=1",
            "--id-label",
            "b=2",
        ])
        .expect("two id-labels must parse");
        assert_eq!(
            cli.args.id_label,
            vec!["a=1".to_string(), "b=2".to_string()]
        );

        // Validated: a malformed label (no `=`) is rejected.
        assert!(
            Cli::try_parse_from(["read-configuration", "--id-label", "noequals"]).is_err(),
            "malformed --id-label must be rejected"
        );
    }

    #[test]
    fn apply_feature_config_sets_mounts() {
        let mut config = json!({"image": "ubuntu"});
        let fc = FeatureContainerConfig {
            mounts: vec!["type=volume,src=v,dst=/v".to_string()],
            ..Default::default()
        };
        apply_feature_config(&mut config, &fc);
        assert_eq!(config["mounts"], json!(["type=volume,src=v,dst=/v"]));
    }

    #[test]
    fn apply_feature_config_sets_capabilities() {
        let mut config = json!({});
        let fc = FeatureContainerConfig {
            cap_add: vec!["SYS_PTRACE".to_string()],
            security_opt: vec!["seccomp=unconfined".to_string()],
            ..Default::default()
        };
        apply_feature_config(&mut config, &fc);
        assert_eq!(config["capAdd"], json!(["SYS_PTRACE"]));
        assert_eq!(config["securityOpt"], json!(["seccomp=unconfined"]));
    }

    #[test]
    fn apply_feature_config_sets_privileged_and_init() {
        let mut config = json!({});
        let fc = FeatureContainerConfig {
            privileged: true,
            init: true,
            ..Default::default()
        };
        apply_feature_config(&mut config, &fc);
        assert_eq!(config["privileged"], json!(true));
        assert_eq!(config["init"], json!(true));
    }

    #[test]
    fn apply_feature_config_skips_false_bools() {
        let mut config = json!({});
        let fc = FeatureContainerConfig::default();
        apply_feature_config(&mut config, &fc);
        assert!(config.get("privileged").is_none());
        assert!(config.get("init").is_none());
    }

    #[test]
    fn apply_feature_config_sets_container_env() {
        let mut config = json!({});
        let fc = FeatureContainerConfig {
            container_env: HashMap::from([("KEY".to_string(), "val".to_string())]),
            ..Default::default()
        };
        apply_feature_config(&mut config, &fc);
        assert_eq!(config["containerEnv"]["KEY"], json!("val"));
    }

    #[test]
    fn apply_feature_config_sets_customizations() {
        let mut config = json!({});
        let fc = FeatureContainerConfig {
            customizations: json!({"vscode": {"settings": {}}}),
            ..Default::default()
        };
        apply_feature_config(&mut config, &fc);
        assert_eq!(
            config["customizations"],
            json!({"vscode": {"settings": {}}})
        );
    }

    #[test]
    fn apply_feature_config_skips_empty_fields() {
        let mut config = json!({"image": "ubuntu"});
        let fc = FeatureContainerConfig::default();
        apply_feature_config(&mut config, &fc);
        assert!(config.get("mounts").is_none());
        assert!(config.get("capAdd").is_none());
        assert!(config.get("containerEnv").is_none());
    }

    #[test]
    fn apply_lifecycle_empty_entries_no_change() {
        let mut config = json!({});
        apply_lifecycle_field(&mut config, "onCreateCommand", &[]);
        assert!(config.get("onCreateCommand").is_none());
    }

    #[test]
    fn apply_lifecycle_single_entry_uses_command_directly() {
        let mut config = json!({});
        let entries = vec![LifecycleEntry {
            origin: "devcontainer.json".to_string(),
            command: json!("echo hello"),
        }];
        apply_lifecycle_field(&mut config, "onCreateCommand", &entries);
        assert_eq!(config["onCreateCommand"], json!("echo hello"));
    }

    #[test]
    fn apply_lifecycle_multiple_entries_uses_object() {
        let mut config = json!({});
        let entries = vec![
            LifecycleEntry {
                origin: "feature-a".to_string(),
                command: json!("echo a"),
            },
            LifecycleEntry {
                origin: "devcontainer.json".to_string(),
                command: json!("echo b"),
            },
        ];
        apply_lifecycle_field(&mut config, "postCreateCommand", &entries);
        assert_eq!(
            config["postCreateCommand"],
            json!({"feature-a": "echo a", "devcontainer.json": "echo b"})
        );
    }

    #[test]
    fn apply_feature_config_lifecycle_integration() {
        let mut config = json!({});
        let fc = FeatureContainerConfig {
            lifecycle: FeatureLifecycle {
                on_create: vec![LifecycleEntry {
                    origin: "feat".to_string(),
                    command: json!("setup"),
                }],
                post_create: vec![
                    LifecycleEntry {
                        origin: "feat".to_string(),
                        command: json!("init"),
                    },
                    LifecycleEntry {
                        origin: "devcontainer.json".to_string(),
                        command: json!("start"),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        apply_feature_config(&mut config, &fc);
        assert_eq!(config["onCreateCommand"], json!("setup"));
        assert_eq!(
            config["postCreateCommand"],
            json!({"feat": "init", "devcontainer.json": "start"})
        );
        assert!(config.get("postStartCommand").is_none());
    }

    fn make_temp_workspace(config_content: &str) -> PathBuf {
        let workspace = std::env::temp_dir().join(format!(
            "cella-read-cfg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let dc_dir = workspace.join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).expect("create .devcontainer dir");
        std::fs::write(dc_dir.join("devcontainer.json"), config_content)
            .expect("write devcontainer.json");
        workspace
    }

    #[tokio::test]
    async fn execute_basic_read_succeeds() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            additional_features: None,
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_override_config_uses_override_file() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let override_path = workspace.join("override.json");
        std::fs::write(&override_path, r#"{"image": "alpine:3.18"}"#).unwrap();

        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: Some(override_path),
            id_label: Vec::new(),
            container_id: None,
            additional_features: None,
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_additional_features_valid_json() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            additional_features: Some(
                r#"{"ghcr.io/devcontainers/features/git:1": {}}"#.to_string(),
            ),
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_additional_features_invalid_json_returns_error() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            additional_features: Some("not valid json".to_string()),
        };
        let err = args.execute().await.unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_additional_features_non_object_returns_error() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            additional_features: Some(r#"["not", "an", "object"]"#.to_string()),
        };
        let err = args.execute().await.unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_include_merged_configuration_no_features() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: true,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            additional_features: None,
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
