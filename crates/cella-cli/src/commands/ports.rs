use clap::Args;
use tracing::debug;

use cella_backend::ContainerInfo;

/// View and manage port forwarding for dev containers.
#[derive(Args)]
pub struct PortsArgs {
    /// Show ports across all worktrees/containers.
    #[arg(long)]
    all: bool,
}

impl PortsArgs {
    pub async fn execute(
        self,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Try querying the daemon first for dynamic port info
        if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
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

        // Fall back to the selected backend for static port bindings
        print_backend_ports(self.all, backend).await
    }
}

fn print_daemon_ports(ports: &[cella_port::protocol::ForwardedPortDetail]) {
    use crate::table::{Column, Table};

    let mut table = Table::new(vec![
        Column::shrinkable("CONTAINER"),
        Column::fixed("PORT"),
        Column::fixed("PROCESS"),
        Column::fixed("HOST PORT"),
        Column::fixed("URL"),
    ]);

    for port in ports {
        table.add_row(vec![
            port.container_name.clone(),
            port.container_port.to_string(),
            port.process.as_deref().unwrap_or("-").to_string(),
            port.host_port.to_string(),
            port.url.clone(),
        ]);
    }

    table.eprint();
}

async fn print_backend_ports(
    all: bool,
    backend: Option<&crate::backend::BackendChoice>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = super::resolve_backend_for_command(backend, None)?;
    let containers = client.list_cella_containers(true).await?;

    // Check if any container is compose-managed and try compose ps
    if let Some(compose_container) = containers
        .iter()
        .find(|c| cella_compose::discovery::is_compose_container(&c.labels))
        && let Some(project_name) =
            cella_compose::discovery::compose_project_from_labels(&compose_container.labels)
        && try_print_compose_ports(project_name).await
    {
        return Ok(());
    }

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

/// Try to print compose service ports via `docker compose ps --format json`.
///
/// Returns `true` if ports were printed, `false` to fall back to generic listing.
async fn try_print_compose_ports(project_name: &str) -> bool {
    use crate::table::{Column, Table};

    let cmd = cella_compose::ComposeCommand::from_project_name(project_name);
    let statuses = match cmd.ps_json().await {
        Ok(s) => s,
        Err(e) => {
            debug!("docker compose ps failed, falling back: {e}");
            return false;
        }
    };

    let has_ports = statuses
        .iter()
        .any(|s| !s.publishers.is_empty() && s.state == "running");

    if !has_ports {
        return false;
    }

    let mut table = Table::new(vec![
        Column::shrinkable("SERVICE"),
        Column::fixed("PORT"),
        Column::fixed("HOST PORT"),
        Column::fixed("PROTOCOL"),
        Column::fixed("URL"),
    ]);

    for svc in &statuses {
        if svc.state != "running" {
            continue;
        }
        for pub_port in &svc.publishers {
            if pub_port.published_port == 0 {
                continue;
            }
            let url = if pub_port.url.is_empty() || pub_port.url == "0.0.0.0" {
                format!("localhost:{}", pub_port.published_port)
            } else {
                format!("{}:{}", pub_port.url, pub_port.published_port)
            };
            table.add_row(vec![
                svc.service.clone(),
                pub_port.target_port.to_string(),
                pub_port.published_port.to_string(),
                pub_port.protocol.clone(),
                url,
            ]);
        }
    }

    table.eprint();
    true
}

fn print_all_container_ports(containers: &[ContainerInfo], is_orbstack: bool) {
    use crate::table::{Column, Table};

    let mut table = if is_orbstack {
        Table::new(vec![
            Column::shrinkable("WORKTREE"),
            Column::fixed("PORT"),
            Column::fixed("PROCESS"),
            Column::fixed("URL"),
        ])
    } else {
        Table::new(vec![
            Column::shrinkable("WORKTREE"),
            Column::fixed("PORT"),
            Column::fixed("PROCESS"),
            Column::fixed("HOST PORT"),
            Column::fixed("URL"),
        ])
    };

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
                table.add_row(vec![
                    name.clone(),
                    port_binding.container_port.to_string(),
                    "-".to_string(),
                    format!(
                        "{}.orb.local:{}",
                        container.name, port_binding.container_port
                    ),
                ]);
            } else {
                let host_port = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| p.to_string());
                let url = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| format!("localhost:{p}"));
                table.add_row(vec![
                    name.clone(),
                    port_binding.container_port.to_string(),
                    "-".to_string(),
                    host_port,
                    url,
                ]);
            }
        }
    }

    table.eprint();
}

fn print_container_ports(containers: &[ContainerInfo], is_orbstack: bool) {
    use crate::table::{Column, Table};

    let mut table = if is_orbstack {
        Table::new(vec![
            Column::fixed("PORT"),
            Column::fixed("PROCESS"),
            Column::fixed("URL"),
        ])
    } else {
        Table::new(vec![
            Column::fixed("PORT"),
            Column::fixed("PROCESS"),
            Column::fixed("HOST PORT"),
            Column::fixed("URL"),
        ])
    };

    for container in containers {
        for port_binding in &container.ports {
            if is_orbstack {
                table.add_row(vec![
                    port_binding.container_port.to_string(),
                    "-".to_string(),
                    format!(
                        "{}.orb.local:{}",
                        container.name, port_binding.container_port
                    ),
                ]);
            } else {
                let host_port = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| p.to_string());
                let url = port_binding
                    .host_port
                    .map_or_else(|| "-".to_string(), |p| format!("localhost:{p}"));
                table.add_row(vec![
                    port_binding.container_port.to_string(),
                    "-".to_string(),
                    host_port,
                    url,
                ]);
            }
        }
    }

    table.eprint();
}
