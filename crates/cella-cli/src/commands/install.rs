use std::path::PathBuf;

use clap::Args;
use tracing::debug;

use cella_backend::ContainerTarget;
use cella_orchestrator::tool_install::ToolName;

use crate::picker;

/// Install tools into the running dev container.
///
/// With no arguments, shows an interactive selector. Already-installed tools
/// are shown but disabled. Specify tool names or `--all` for non-interactive use.
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

        let (installed, not_installed) =
            probe_install_status(client.as_ref(), &container.id, &remote_user).await;

        // Determine which tools to install
        let to_install = if self.all {
            if not_installed.is_empty() {
                eprintln!("All tools are already installed.");
                return Ok(());
            }
            not_installed
        } else if self.tools.is_empty() {
            select_tools_interactive(&installed, &not_installed)?
        } else {
            resolve_tool_args(&self.tools, &installed)?
        };

        if to_install.is_empty() {
            return Ok(());
        }

        let settings =
            load_settings_with_version(&container, self.version.as_deref(), &to_install)?;

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
) -> Result<cella_config::CellaConfig, Box<dyn std::error::Error + Send + Sync>> {
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
                ToolName::Tmux => {
                    return Err(
                        "tmux does not support --version (installed via system package manager)"
                            .into(),
                    );
                }
            }
        }
    }
    Ok(settings)
}
