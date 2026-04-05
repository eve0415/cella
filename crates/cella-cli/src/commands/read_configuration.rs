use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use cella_config::devcontainer::resolve;

/// Read and output the resolved devcontainer configuration.
#[derive(Args)]
pub struct ReadConfigurationArgs {
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
}

impl ReadConfigurationArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let cwd = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        let resolved = resolve::config(&cwd, self.config.as_deref())?;

        // Determine deviceContainerType
        let device_type = if resolved.config.get("dockerComposeFile").is_some() {
            "dockerCompose"
        } else if resolved.config.get("build").is_some()
            || resolved.config.get("dockerFile").is_some()
        {
            "dockerfile"
        } else {
            "image"
        };

        // Compute workspace folder
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

        // Include merged configuration if requested.
        // Full feature resolution requires Docker and network access;
        // for now output the resolved configuration itself.
        if self.include_merged_configuration {
            output["mergeConfiguration"] = output["configuration"].clone();
        }

        println!("{}", serde_json::to_string_pretty(&output)?);
        Ok(())
    }
}
