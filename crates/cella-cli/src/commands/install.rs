use std::path::PathBuf;

use clap::Args;
use tracing::debug;

use cella_backend::ContainerTarget;
use cella_orchestrator::tool_install::ToolName;

use crate::picker;

/// Install tools into the running dev container.
///
/// With no arguments, shows an interactive selector. Already-installed tools
/// are hidden. Specify tool names or `--all` for non-interactive use.
#[derive(Args)]
pub struct InstallArgs {
    /// Tools to install (e.g. `claude-code`, `codex`, `gemini`, `nvim`, `tmux`).
    pub tools: Vec<String>,

    /// Install all available tools.
    #[arg(long)]
    pub all: bool,

    /// Override the version to install.
    #[arg(long)]
    pub version: Option<String>,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Target container by ID.
    #[arg(long)]
    container_id: Option<String>,

    /// Target container by name.
    #[arg(long)]
    container_name: Option<String>,

    /// Target container by label.
    #[arg(long)]
    id_label: Option<String>,

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    service: Option<String>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,
}

impl InstallArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Validate flag combinations before any I/O
        if let Some(ref ver) = self.version {
            validate_version_flag(ver, &self.tools, self.all)?;
        }

        let client = self.backend.resolve_client().await?;

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: self.id_label,
            workspace_folder: self.workspace_folder,
        };

        let has_explicit = picker::has_explicit_target(&target);
        let container = match target.resolve(client.as_ref(), true).await {
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
        };
        let container =
            super::resolve_service_container(client.as_ref(), container, self.service.as_deref())
                .await?;

        super::ensure_cella_daemon().await;

        let remote_user = container
            .labels
            .get("dev.cella.remote_user")
            .cloned()
            .or_else(|| container.container_user.clone())
            .unwrap_or_else(|| "root".to_string());

        // When --version is specified, skip install-status checks — the user
        // wants to install/upgrade a specific version regardless.
        let to_install = if self.version.is_some() {
            // --version requires exactly one tool name (validated above)
            vec![ToolName::from_config_name(&self.tools[0]).unwrap()]
        } else {
            let (installed, not_installed) =
                probe_install_status(client.as_ref(), &container.id, &remote_user).await;
            if self.all {
                if not_installed.is_empty() {
                    eprintln!("All tools are already installed.");
                    return Ok(());
                }
                not_installed
            } else if self.tools.is_empty() {
                select_tools_interactive(&installed, &not_installed)?
            } else {
                resolve_tool_args(&self.tools, &installed)?
            }
        };

        if to_install.is_empty() {
            return Ok(());
        }

        let settings = load_settings_with_version(&container, self.version.as_deref(), &to_install);

        // Detect shell for verification
        let shell = cella_orchestrator::shell_detect::detect_shell(
            client.as_ref(),
            &container.id,
            &remote_user,
        )
        .await;

        // Install using the shared install_tools machinery
        let progress = crate::progress::Progress::new(true, crate::progress::Verbosity::Normal);
        let (sender, renderer) = crate::progress::bridge(&progress);
        let spec = cella_orchestrator::tool_install::InstallSpec {
            settings: &settings,
            tools: &to_install,
            probed_env: None,
        };
        cella_orchestrator::tool_install::install_tools(
            client.as_ref(),
            &container.id,
            &remote_user,
            &shell,
            &spec,
            &sender,
        )
        .await;
        drop(sender);
        let _ = renderer.await;

        Ok(())
    }
}

fn select_tools_interactive(
    installed: &[ToolName],
    not_installed: &[ToolName],
) -> Result<Vec<ToolName>, Box<dyn std::error::Error + Send + Sync>> {
    if not_installed.is_empty() {
        eprintln!("All tools are already installed.");
        return Ok(Vec::new());
    }

    let mut options: Vec<String> = Vec::new();
    let mut tool_map: Vec<ToolName> = Vec::new();

    for tool in ToolName::ALL {
        if installed.contains(tool) {
            continue;
        }
        options.push(tool.display_name().to_string());
        tool_map.push(*tool);
    }

    let selected = inquire::MultiSelect::new("Select tools to install:", options)
        .with_help_message("Already installed tools are not shown")
        .prompt();

    match selected {
        Ok(choices) => {
            let tools: Vec<ToolName> = choices
                .iter()
                .filter_map(|choice| {
                    tool_map
                        .iter()
                        .find(|t| t.display_name() == choice.as_str())
                        .copied()
                })
                .collect();
            Ok(tools)
        }
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn resolve_tool_args(
    args: &[String],
    installed: &[ToolName],
) -> Result<Vec<ToolName>, Box<dyn std::error::Error + Send + Sync>> {
    let mut tools = Vec::new();
    for arg in args {
        let Some(tool) = ToolName::from_config_name(arg) else {
            return Err(format!(
                "Unknown tool: {arg}. Valid tools: {}",
                ToolName::ALL
                    .iter()
                    .map(|t| t.config_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        };
        if installed.contains(&tool) {
            debug!("{} is already installed, skipping", tool.display_name());
            eprintln!("{} is already installed.", tool.display_name());
            continue;
        }
        tools.push(tool);
    }
    Ok(tools)
}

async fn probe_install_status(
    client: &dyn cella_backend::ContainerBackend,
    container_id: &str,
    remote_user: &str,
) -> (Vec<ToolName>, Vec<ToolName>) {
    let mut installed = Vec::new();
    let mut not_installed = Vec::new();
    for tool in ToolName::ALL {
        if cella_orchestrator::tool_install::is_tool_installed(
            client,
            container_id,
            remote_user,
            *tool,
            None,
        )
        .await
        {
            installed.push(*tool);
        } else {
            not_installed.push(*tool);
        }
    }
    (installed, not_installed)
}

fn load_settings_with_version(
    container: &cella_backend::ContainerInfo,
    version: Option<&str>,
    tools: &[ToolName],
) -> cella_config::CellaConfig {
    let workspace_path = container
        .labels
        .get("dev.cella.workspace_path")
        .map(PathBuf::from);
    let resolved = workspace_path
        .as_deref()
        .and_then(|p| cella_config::devcontainer::resolve::config(p, None).ok());
    let mut settings = cella_config::CellaConfig::load(
        workspace_path
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(".")),
        resolved.as_ref(),
    )
    .unwrap_or_default();

    if let Some(ver) = version {
        for tool in tools {
            match tool {
                ToolName::ClaudeCode => settings.tools.claude_code.version = ver.to_string(),
                ToolName::Codex => settings.tools.codex.version = ver.to_string(),
                ToolName::Gemini => settings.tools.gemini.version = ver.to_string(),
                ToolName::Nvim => settings.tools.nvim.version = ver.to_string(),
                ToolName::Tmux => unreachable!("validated in validate_version_flag"),
            }
        }
    }
    settings
}

fn validate_version_flag(
    _version: &str,
    tools: &[String],
    all: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if all {
        return Err("--version cannot be combined with --all".into());
    }
    if tools.is_empty() {
        return Err("--version requires specifying a tool name".into());
    }
    if tools.len() > 1 {
        return Err("--version can only be used with a single tool".into());
    }
    let name = &tools[0];
    let Some(tool) = ToolName::from_config_name(name) else {
        return Err(format!("Unknown tool: {name}").into());
    };
    if tool == ToolName::Tmux {
        return Err(
            "tmux does not support --version (installed via system package manager)".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tool_args_valid_names() {
        let args = vec!["claude-code".to_string(), "nvim".to_string()];
        let result = resolve_tool_args(&args, &[]).unwrap();
        assert_eq!(result, vec![ToolName::ClaudeCode, ToolName::Nvim]);
    }

    #[test]
    fn resolve_tool_args_unknown_name() {
        let args = vec!["vim".to_string()];
        let err = resolve_tool_args(&args, &[]).unwrap_err();
        assert!(err.to_string().contains("Unknown tool: vim"));
    }

    #[test]
    fn resolve_tool_args_skips_installed() {
        let args = vec!["claude-code".to_string(), "nvim".to_string()];
        let installed = vec![ToolName::ClaudeCode];
        let result = resolve_tool_args(&args, &installed).unwrap();
        assert_eq!(result, vec![ToolName::Nvim]);
    }

    #[test]
    fn resolve_tool_args_all_installed() {
        let args = vec!["codex".to_string()];
        let installed = vec![ToolName::Codex];
        let result = resolve_tool_args(&args, &installed).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn validate_version_single_tool_ok() {
        let tools = vec!["claude-code".to_string()];
        assert!(validate_version_flag("1.0.0", &tools, false).is_ok());
    }

    #[test]
    fn validate_version_rejects_all() {
        let tools = vec!["claude-code".to_string()];
        let err = validate_version_flag("1.0.0", &tools, true).unwrap_err();
        assert!(err.to_string().contains("--all"));
    }

    #[test]
    fn validate_version_rejects_multiple_tools() {
        let tools = vec!["claude-code".to_string(), "codex".to_string()];
        let err = validate_version_flag("1.0.0", &tools, false).unwrap_err();
        assert!(err.to_string().contains("single tool"));
    }

    #[test]
    fn validate_version_rejects_tmux() {
        let tools = vec!["tmux".to_string()];
        let err = validate_version_flag("1.0.0", &tools, false).unwrap_err();
        assert!(err.to_string().contains("tmux"));
    }

    #[test]
    fn validate_version_rejects_no_tools() {
        let err = validate_version_flag("1.0.0", &[], false).unwrap_err();
        assert!(err.to_string().contains("requires"));
    }
}
