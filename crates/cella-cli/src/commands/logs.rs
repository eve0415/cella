use std::path::PathBuf;

use clap::Args;

/// View logs from the dev container.
#[derive(Args)]
pub struct LogsArgs {
    /// Follow log output (streams new logs in real-time).
    #[arg(short, long)]
    follow: bool,

    /// Show captured lifecycle command output from the last `cella up`.
    #[arg(long)]
    lifecycle: bool,

    /// Show daemon log output.
    #[arg(long)]
    daemon: bool,

    /// Number of lines to show from the end of the logs.
    #[arg(long, default_value = "100")]
    tail: u32,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Filter logs to a specific compose service (compose only).
    #[arg(long)]
    service: Option<String>,
}

impl LogsArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        if self.daemon {
            return self.show_daemon_logs();
        }
        if self.lifecycle {
            return self.show_lifecycle_logs();
        }
        self.show_container_logs().await
    }

    async fn show_container_logs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let client = super::connect_docker(self.docker_host.as_deref())?;

        let cwd = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        let container = client
            .find_container(&cwd)
            .await?
            .ok_or("No cella container found for this workspace")?;

        // Docker Compose: use docker compose logs for all/specific services
        if let Some(project_name) =
            cella_compose::discovery::compose_project_from_labels(&container.labels)
        {
            let compose_cmd = cella_compose::ComposeCommand::from_project_name(project_name);
            let services = self.service.as_ref().map(|s| vec![s.clone()]);
            compose_cmd
                .logs(self.follow, self.tail, services.as_deref())
                .await
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!("docker compose logs failed: {e}").into()
                })?;
            return Ok(());
        }

        if self.follow {
            // Use docker CLI for follow mode (bollard streaming is complex)
            let status = std::process::Command::new("docker")
                .args([
                    "logs",
                    "-f",
                    "--tail",
                    &self.tail.to_string(),
                    &container.id,
                ])
                .status()?;
            if !status.success() {
                return Err(format!(
                    "docker logs exited with code {}",
                    status.code().unwrap_or(-1)
                )
                .into());
            }
        } else {
            let logs = client.container_logs(&container.id, self.tail).await?;
            print!("{logs}");
        }
        Ok(())
    }

    fn show_daemon_logs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(data_dir) = cella_env::git_credential::cella_data_dir() else {
            return Err("Cannot determine cella data directory".into());
        };
        let log_path = data_dir.join("daemon.log");
        if !log_path.exists() {
            eprintln!("No daemon log found at {}", log_path.display());
            return Ok(());
        }
        let content = std::fs::read_to_string(&log_path)?;
        // Show last N lines
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(self.tail as usize);
        for line in &lines[start..] {
            println!("{line}");
        }
        Ok(())
    }

    fn show_lifecycle_logs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self; // tail unused for lifecycle logs
        let Some(data_dir) = cella_env::git_credential::cella_data_dir() else {
            return Err("Cannot determine cella data directory".into());
        };
        let logs_dir = data_dir.join("logs");
        if !logs_dir.exists() {
            eprintln!("No lifecycle logs found. Run `cella up` first.");
            return Ok(());
        }

        // Find the most recent container log directory
        let mut entries: Vec<_> = std::fs::read_dir(&logs_dir)?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
            .collect();
        entries
            .sort_by_key(|e| std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok())));

        let Some(latest) = entries.first() else {
            eprintln!("No lifecycle logs found.");
            return Ok(());
        };

        eprintln!(
            "Lifecycle logs for {}:",
            latest.file_name().to_string_lossy()
        );
        eprintln!();

        // Read and print each phase log file
        let mut phase_files: Vec<_> = std::fs::read_dir(latest.path())?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
            .collect();
        phase_files.sort_by_key(std::fs::DirEntry::file_name);

        for file in phase_files {
            let phase_name = file
                .path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let content = std::fs::read_to_string(file.path())?;
            if !content.is_empty() {
                eprintln!("[{phase_name}]");
                eprint!("{content}");
                eprintln!();
            }
        }
        Ok(())
    }
}
