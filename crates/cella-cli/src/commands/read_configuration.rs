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

    /// Target container by label.
    #[arg(long = "id-label")]
    id_label: Option<String>,

    /// Target container by ID.
    #[arg(long = "container-id")]
    container_id: Option<String>,

    /// Additional features as JSON string.
    #[arg(long = "additional-features")]
    additional_features: Option<String>,
}

impl ReadConfigurationArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let has_container_target = self.id_label.is_some() || self.container_id.is_some();

        let config_path = self.override_config.as_deref().or(self.config.as_deref());

        let (mut resolved, cwd) = if has_container_target {
            let client = self.backend.resolve_client().await?;
            let target = ContainerTarget {
                container_id: self.container_id.clone(),
                container_name: None,
                id_label: self.id_label.clone(),
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
            let extra: serde_json::Value = serde_json::from_str(additional)
                .map_err(|e| format!("--additional-features: invalid JSON: {e}"))?;
            let obj = extra
                .as_object()
                .ok_or("--additional-features must be a JSON object")?;
            let features = resolved
                .config
                .as_object_mut()
                .expect("config is always an object")
                .entry("features")
                .or_insert_with(|| json!({}));
            let features_obj = features
                .as_object_mut()
                .ok_or("existing features field is not an object")?;
            for (k, v) in obj {
                features_obj.insert(k.clone(), v.clone());
            }
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

async fn resolve_merged_config(
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
    };
    let rf =
        cella_features::resolve_features(config, config_path, &platform, &cache, &base_ctx, false)
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
