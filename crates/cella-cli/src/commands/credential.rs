use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use tracing::{info, warn};

use cella_backend::{ContainerTarget, ExecOptions, FileToUpload};

/// Manage credential forwarding for dev containers.
#[derive(Args)]
pub struct CredentialArgs {
    #[command(subcommand)]
    command: CredentialCommand,
}

#[derive(Subcommand)]
enum CredentialCommand {
    /// Sync host credentials into a running container.
    Sync(SyncArgs),
    /// Show credential forwarding status.
    Status(StatusArgs),
}

#[derive(Args)]
struct SyncArgs {
    /// Which tool's credentials to sync.
    tool: CredentialTool,
    /// Target a specific container by ID (default: auto-detect from cwd).
    #[arg(long)]
    container: Option<String>,
    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,
}

#[derive(Args)]
struct StatusArgs {
    /// Target a specific container by ID.
    #[arg(long)]
    container: Option<String>,
    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,
}

#[derive(Clone, ValueEnum)]
enum CredentialTool {
    /// GitHub CLI credentials.
    Gh,
    // Future: Claude, Codex, Gemini
}

impl CredentialArgs {
    pub async fn execute(
        self,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            CredentialCommand::Sync(args) => run_sync(args, backend).await,
            CredentialCommand::Status(args) => run_status(args, backend).await,
        }
    }
}

async fn run_sync(
    args: SyncArgs,
    backend: Option<&crate::backend::BackendChoice>,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.tool {
        CredentialTool::Gh => sync_gh(args.container, args.workspace_folder, backend).await,
    }
}

async fn sync_gh(
    container_id_override: Option<String>,
    workspace_folder: Option<PathBuf>,
    backend: Option<&crate::backend::BackendChoice>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = super::resolve_backend_for_command(backend, None)?;

    let cwd = super::resolve_workspace_folder(workspace_folder.as_deref())?;

    // Find target container
    let target = ContainerTarget {
        container_id: container_id_override,
        container_name: None,
        id_label: None,
        workspace_folder: Some(cwd.clone()),
    };
    let container = target.resolve(client.as_ref(), true).await?;

    // Read remote_user from label
    let remote_user = container
        .labels
        .get("dev.cella.remote_user")
        .cloned()
        .unwrap_or_else(|| "root".to_string());

    let config_dir = cella_env::gh_credential::gh_config_dir_for_user(&remote_user);

    // Prepare fresh credentials from host
    let gh_creds = cella_env::gh_credential::prepare_gh_credentials(&cwd, &remote_user)
        .ok_or("gh CLI is not installed or not authenticated on the host")?;

    // Check if files already exist and differ
    let check_cmd = cella_env::gh_credential::gh_config_exists_in_container(&config_dir);
    let exists = client
        .exec_command(
            &container.id,
            &ExecOptions {
                cmd: check_cmd,
                user: Some(remote_user.clone()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0);

    if exists {
        eprintln!(
            "\x1b[33mWARNING:\x1b[0m gh config already exists in container (may have been modified in-container). Overwriting."
        );
    }

    // Create config directory
    client
        .exec_command(
            &container.id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("mkdir -p {config_dir} && chmod 700 {config_dir}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    // Upload files
    let docker_files: Vec<FileToUpload> = gh_creds
        .file_uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect();

    client.upload_files(&container.id, &docker_files).await?;

    // Fix ownership
    let _ = client
        .exec_command(
            &container.id,
            &ExecOptions {
                cmd: vec![
                    "chown".to_string(),
                    "-R".to_string(),
                    format!("{remote_user}:{remote_user}"),
                    config_dir,
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    info!("Synced gh CLI credentials into container");
    eprintln!("gh CLI credentials synced successfully.");

    Ok(())
}

async fn run_status(
    args: StatusArgs,
    backend: Option<&crate::backend::BackendChoice>,
) -> Result<(), Box<dyn std::error::Error>> {
    let redactor = cella_doctor::redact::Redactor::new();

    // Host section: check gh auth status
    eprintln!("Host:");
    let gh_status = cella_env::gh_credential::probe_host_gh_status();
    if !gh_status.installed {
        eprintln!("  gh CLI: not installed");
    } else if gh_status.authenticated {
        if let Some(ref output) = gh_status.status_output {
            let redacted = redactor.redact(output);
            for line in redacted.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    eprintln!("  {trimmed}");
                }
            }
        }
    } else {
        eprintln!("  gh CLI: not authenticated");
    }

    // Config section
    let cwd = if let Some(ref wf) = args.workspace_folder {
        wf.canonicalize().unwrap_or_else(|_| wf.clone())
    } else {
        std::env::current_dir()?
    };

    let settings = cella_config::settings::Settings::load(&cwd);
    eprintln!();
    eprintln!("Settings:");
    eprintln!(
        "  credentials.gh = {} ({})",
        settings.credentials.gh,
        if settings.credentials.gh {
            "auto-forward"
        } else {
            "disabled"
        }
    );

    // Container section
    let client = super::resolve_backend_for_command(backend, None)?;
    let target = ContainerTarget {
        container_id: args.container,
        container_name: None,
        id_label: None,
        workspace_folder: Some(cwd),
    };

    eprintln!();
    eprintln!("Container:");
    match target.resolve(client.as_ref(), false).await {
        Ok(container) => {
            let remote_user = container
                .labels
                .get("dev.cella.remote_user")
                .cloned()
                .unwrap_or_else(|| "root".to_string());

            let config_dir = cella_env::gh_credential::gh_config_dir_for_user(&remote_user);
            let check_cmd = cella_env::gh_credential::gh_config_exists_in_container(&config_dir);

            let has_creds = client
                .exec_command(
                    &container.id,
                    &ExecOptions {
                        cmd: check_cmd,
                        user: Some(remote_user.clone()),
                        env: None,
                        working_dir: None,
                    },
                )
                .await
                .is_ok_and(|r| r.exit_code == 0);

            let short_id = &container.id[..12.min(container.id.len())];
            eprintln!("  Container: {short_id}");
            eprintln!("  User: {remote_user}");
            if has_creds {
                eprintln!("  gh credentials: present ({config_dir}/hosts.yml)");
            } else {
                eprintln!("  gh credentials: not found");
            }
        }
        Err(e) => {
            warn!("Could not find container: {e}");
            eprintln!("  No running container found for this workspace");
        }
    }

    Ok(())
}
