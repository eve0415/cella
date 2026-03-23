use clap::Args;
use tracing::debug;

/// View and manage port forwarding for dev containers.
#[derive(Args)]
pub struct PortsArgs {
    /// Show ports across all worktrees/containers.
    #[arg(long)]
    all: bool,
}

impl PortsArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        // Try querying the daemon first for dynamic port info
        if let Some(mgmt_sock) = cella_env::git_credential::daemon_management_socket_path()
            && mgmt_sock.exists()
        {
            match cella_daemon::management::send_management_request(
                &mgmt_sock,
                &cella_port::protocol::ManagementRequest::QueryPorts,
            )
            .await
            {
                Ok(cella_port::protocol::ManagementResponse::Ports { ports }) => {
                    if !ports.is_empty() {
                        print_daemon_ports(&ports);
                        return Ok(());
                    }
                    // Fall through to Docker static ports
                }
                Ok(_) => {
                    debug!("Unexpected response from daemon");
                }
                Err(e) => {
                    debug!("Daemon query failed, falling back to Docker: {e}");
                }
            }
        }

        // Fall back to Docker API for static port bindings
        print_docker_ports(self.all).await
    }
}

fn print_daemon_ports(ports: &[cella_port::protocol::ForwardedPortDetail]) {
    eprintln!(
        "{:<20} {:<8} {:<12} {:<12} URL",
        "CONTAINER", "PORT", "PROCESS", "HOST PORT"
    );

    for port in ports {
        let process = port.process.as_deref().unwrap_or("-");
        eprintln!(
            "{:<20} {:<8} {:<12} {:<12} {}",
            truncate_name(&port.container_name, 20),
            port.container_port,
            process,
            port.host_port,
            port.url,
        );
    }
}

fn truncate_name(name: &str, max_len: usize) -> &str {
    if name.len() > max_len {
        &name[..max_len]
    } else {
        name
    }
}

async fn print_docker_ports(all: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = cella_docker::DockerClient::connect()?;
    let containers = client.list_cella_containers(true).await?;

    let has_ports = containers.iter().any(|c| !c.ports.is_empty());

    if containers.is_empty() || !has_ports {
        eprintln!("No ports detected.");
        return Ok(());
    }

    let runtime = cella_env::platform::detect_runtime();
    let is_orbstack = runtime == cella_env::DockerRuntime::OrbStack;

    if all {
        print_all_container_ports(&containers, is_orbstack);
    } else {
        print_container_ports(&containers, is_orbstack);
    }

    Ok(())
}

fn print_all_container_ports(containers: &[cella_docker::ContainerInfo], is_orbstack: bool) {
    if is_orbstack {
        eprintln!("{:<20} {:<8} {:<12} URL", "WORKTREE", "PORT", "PROCESS");
    } else {
        eprintln!(
            "{:<20} {:<8} {:<12} {:<12} URL",
            "WORKTREE", "PORT", "PROCESS", "HOST PORT"
        );
    }

    for container in containers {
        let name = container
            .labels
            .get("dev.cella.workspace_path")
            .and_then(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| container.name.clone());

        for port_binding in &container.ports {
            if is_orbstack {
                eprintln!(
                    "{:<20} {:<8} {:<12} {}.orb.local:{}",
                    name,
                    port_binding.container_port,
                    "-",
                    container.name,
                    port_binding.container_port,
                );
            } else {
                let host_port = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| p.to_string());
                let url = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| format!("localhost:{p}"));
                eprintln!(
                    "{:<20} {:<8} {:<12} {:<12} {}",
                    name, port_binding.container_port, "-", host_port, url,
                );
            }
        }
    }
}

fn print_container_ports(containers: &[cella_docker::ContainerInfo], is_orbstack: bool) {
    if is_orbstack {
        eprintln!("{:<8} {:<12} URL", "PORT", "PROCESS");
    } else {
        eprintln!("{:<8} {:<12} {:<12} URL", "PORT", "PROCESS", "HOST PORT");
    }

    for container in containers {
        for port_binding in &container.ports {
            if is_orbstack {
                eprintln!(
                    "{:<8} {:<12} {}.orb.local:{}",
                    port_binding.container_port, "-", container.name, port_binding.container_port,
                );
            } else {
                let host_port = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| p.to_string());
                let url = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| format!("localhost:{p}"));
                eprintln!(
                    "{:<8} {:<12} {:<12} {}",
                    port_binding.container_port, "-", host_port, url,
                );
            }
        }
    }
}
