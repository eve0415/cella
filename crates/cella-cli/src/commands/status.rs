use clap::Args;
use serde_json::json;

/// Show system-level status overview.
#[derive(Args)]
pub struct StatusArgs {
    /// Output format.
    #[arg(long, value_enum, default_value = "json")]
    output: super::OutputFormat,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,
}

impl StatusArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;

        let worktrees = gather_worktrees();
        let (containers, daemon, docker) = tokio::join!(
            gather_containers(client.as_ref()),
            gather_daemon(),
            gather_docker_version(),
        );

        match self.output {
            super::OutputFormat::Json => {
                let result = json!({
                    "containers": containers,
                    "worktrees": worktrees,
                    "daemon": daemon,
                    "docker": docker,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
            super::OutputFormat::Text => {
                print_text(&containers, &worktrees, &daemon, &docker);
            }
        }

        Ok(())
    }
}

async fn gather_containers(client: &dyn cella_backend::ContainerBackend) -> serde_json::Value {
    let all = client
        .list_cella_containers(false)
        .await
        .unwrap_or_default();
    let running = all
        .iter()
        .filter(|c| matches!(c.state, cella_backend::ContainerState::Running))
        .count();
    let stopped = all
        .iter()
        .filter(|c| matches!(c.state, cella_backend::ContainerState::Stopped))
        .count();
    json!({
        "running": running,
        "stopped": stopped,
        "total": all.len(),
    })
}

fn gather_worktrees() -> serde_json::Value {
    let cwd = std::env::current_dir().unwrap_or_default();
    let Ok(repo_info) = cella_git::discover(&cwd) else {
        return json!({ "linked": 0, "main": 0 });
    };

    let worktrees = cella_git::list(&repo_info.root).unwrap_or_default();
    let main_count = worktrees.iter().filter(|wt| wt.is_main).count();
    let linked = worktrees.len() - main_count;
    json!({
        "linked": linked,
        "main": main_count,
    })
}

async fn gather_daemon() -> serde_json::Value {
    let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
        return json!({ "running": false });
    };

    if !mgmt_sock.exists() {
        return json!({ "running": false });
    }

    match cella_daemon_client::send_management_request(
        &mgmt_sock,
        &cella_protocol::ManagementRequest::QueryStatus,
    )
    .await
    {
        Ok(cella_protocol::ManagementResponse::Status {
            daemon_version,
            daemon_started_at,
            container_count,
            ..
        }) => {
            let uptime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(daemon_started_at);
            json!({
                "running": true,
                "version": daemon_version,
                "uptime_secs": uptime,
                "registered_containers": container_count,
            })
        }
        _ => json!({ "running": false }),
    }
}

async fn gather_docker_version() -> serde_json::Value {
    match tokio::process::Command::new("docker")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            json!({ "available": true, "version": version })
        }
        _ => json!({ "available": false, "version": "" }),
    }
}

fn print_text(
    containers: &serde_json::Value,
    worktrees: &serde_json::Value,
    daemon: &serde_json::Value,
    docker: &serde_json::Value,
) {
    eprintln!(
        "Containers: {} running, {} stopped ({} total)",
        containers["running"], containers["stopped"], containers["total"]
    );
    eprintln!(
        "Worktrees:  {} linked, {} main",
        worktrees["linked"], worktrees["main"]
    );
    let daemon_status = if daemon["running"].as_bool().unwrap_or(false) {
        format!(
            "running (v{}, uptime {}s)",
            daemon["version"].as_str().unwrap_or("?"),
            daemon["uptime_secs"]
        )
    } else {
        "not running".to_string()
    };
    eprintln!("Daemon:     {daemon_status}");
    let docker_status = if docker["available"].as_bool().unwrap_or(false) {
        format!("available ({})", docker["version"].as_str().unwrap_or("?"))
    } else {
        "unavailable".to_string()
    };
    eprintln!("Docker:     {docker_status}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_args_default_json() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "status"]).unwrap();
        assert!(matches!(cli.command, crate::commands::Command::Status(_)));
    }

    #[test]
    fn status_args_text_output() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "status", "--output", "text"]).unwrap();
        assert!(matches!(cli.command, crate::commands::Command::Status(_)));
    }
}
