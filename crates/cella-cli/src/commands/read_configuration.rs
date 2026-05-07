use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use cella_backend::ContainerTarget;
use cella_config::devcontainer::resolve;

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

        let (resolved, cwd) = if has_container_target {
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
            let res = resolve::config(&ws, self.config.as_deref())?;
            (res, ws)
        } else {
            let ws = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let res = resolve::config(&ws, self.config.as_deref())?;
            (res, ws)
        };

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
            output["mergedConfiguration"] = output["configuration"].clone();
        }

        println!("{}", serde_json::to_string_pretty(&output)?);
        Ok(())
    }
}
