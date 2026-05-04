use clap::Args;
use tracing::debug;

use cella_backend::ContainerInfo;

/// View and manage port forwarding for dev containers.
#[derive(Args)]
pub struct PortsArgs {
    /// Show ports across all worktrees/containers.
    #[arg(long)]
    all: bool,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: super::OutputFormat,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,
}

impl PortsArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let is_json = matches!(self.output, super::OutputFormat::Json);

        // Try querying the daemon first for dynamic port info — only when
        // the selected backend uses daemon-managed port forwarding and no
        // custom Docker host is specified (daemon tracks local containers only).
        let is_docker_backend = self
            .backend
            .backend
            .as_ref()
            .is_none_or(|b| matches!(b, crate::backend::BackendChoice::Docker));
        let effective_host = self
            .backend
            .docker_host
            .clone()
            .or_else(|| std::env::var("DOCKER_HOST").ok());
        let has_remote_host = effective_host.as_deref().is_some_and(is_remote_docker_host);
        if is_docker_backend
            && !has_remote_host
            && let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
            && mgmt_sock.exists()
        {
            match cella_daemon_client::send_management_request(
                &mgmt_sock,
                &cella_protocol::ManagementRequest::QueryPorts,
            )
            .await
            {
                Ok(cella_protocol::ManagementResponse::Ports { ports }) => {
                    if !ports.is_empty() {
                        if is_json {
                            print_daemon_ports_json(&ports);
                        } else {
                            print_daemon_ports(&ports);
                        }
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
        if is_json {
            print_backend_ports_json(self.all, &self.backend).await
        } else {
            print_backend_ports(self.all, &self.backend).await
        }
    }
}

/// Returns `true` when the Docker host points to a non-local engine.
///
/// Local aliases (`unix://` sockets, `tcp://localhost`, `tcp://127.0.0.1`)
/// still target the same daemon the cella daemon manages, so the daemon
/// fast-path should remain active for those.
fn is_remote_docker_host(host: &str) -> bool {
    if host.starts_with("unix://") || host.starts_with("npipe://") {
        return false;
    }
    if let Some(rest) = host.strip_prefix("tcp://") {
        let authority = rest.split('/').next().unwrap_or(rest);
        let hostname = authority
            .strip_prefix('[')
            .and_then(|h| h.split_once(']').map(|(host, _)| host))
            .unwrap_or_else(|| authority.split(':').next().unwrap_or(authority));
        return !matches!(hostname, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    }
    // Unknown scheme — assume remote to be safe
    true
}

fn print_daemon_ports(ports: &[cella_protocol::ForwardedPortDetail]) {
    use crate::table::{Column, Table};

    let has_hostnames = ports.iter().any(|p| p.hostname.is_some());

    let mut columns = vec![
        Column::shrinkable("CONTAINER"),
        Column::fixed("PORT"),
        Column::fixed("PROCESS"),
        Column::fixed("HOST PORT"),
    ];
    if has_hostnames {
        columns.push(Column::shrinkable("HOSTNAME"));
    }
    columns.push(Column::fixed("URL"));

    let mut table = Table::new(columns);

    for port in ports {
        let mut row = vec![
            port.container_name.clone(),
            port.container_port.to_string(),
            port.process.as_deref().unwrap_or("-").to_string(),
            port.host_port.to_string(),
        ];
        if has_hostnames {
            row.push(port.hostname.as_deref().unwrap_or("-").to_string());
        }
        row.push(port.url.clone());
        table.add_row(row);
    }

    table.eprint();
}

async fn print_backend_ports(
    all: bool,
    backend_args: &crate::backend::BackendArgs,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = backend_args.resolve_client().await?;
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

fn print_daemon_ports_json(ports: &[cella_protocol::ForwardedPortDetail]) {
    let items: Vec<_> = ports
        .iter()
        .map(|p| {
            serde_json::json!({
                "container_name": p.container_name,
                "container_port": p.container_port,
                "host_port": p.host_port,
                "protocol": format!("{:?}", p.protocol).to_lowercase(),
                "process": p.process,
                "url": p.url,
                "hostname": p.hostname,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&items).unwrap_or_default()
    );
}

async fn print_backend_ports_json(
    all: bool,
    backend_args: &crate::backend::BackendArgs,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = backend_args.resolve_client().await?;
    let containers = client.list_cella_containers(true).await?;

    let filter_containers: Vec<_> = if all {
        containers.iter().collect()
    } else {
        containers.iter().filter(|c| !c.ports.is_empty()).collect()
    };

    let items: Vec<_> = filter_containers
        .iter()
        .flat_map(|c| {
            c.ports.iter().map(|p| {
                serde_json::json!({
                    "container_name": c.name,
                    "container_port": p.container_port,
                    "host_port": p.host_port,
                    "protocol": p.protocol,
                })
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&items).unwrap_or_default()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cella_backend::{BackendKind, ContainerState, PortBinding};
    use std::collections::HashMap;

    fn container(
        name: &str,
        workspace_path: Option<&str>,
        ports: Vec<PortBinding>,
    ) -> ContainerInfo {
        let mut labels = HashMap::new();
        if let Some(path) = workspace_path {
            labels.insert("dev.cella.workspace_path".to_string(), path.to_string());
        }
        ContainerInfo {
            id: format!("{name}-id"),
            name: name.to_string(),
            state: ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports,
            created_at: None,
            container_user: None,
            image: None,
            mounts: Vec::new(),
            backend: BackendKind::Docker,
        }
    }

    fn port(container_port: u16, host_port: Option<u16>, protocol: &str) -> PortBinding {
        PortBinding {
            container_port,
            host_port,
            protocol: protocol.to_string(),
        }
    }

    #[test]
    fn remote_docker_host_detects_local_aliases() {
        assert!(!is_remote_docker_host("unix:///var/run/docker.sock"));
        assert!(!is_remote_docker_host("npipe:////./pipe/docker_engine"));
        assert!(!is_remote_docker_host("tcp://localhost:2375"));
        assert!(!is_remote_docker_host("tcp://127.0.0.1:2375"));
        assert!(!is_remote_docker_host("tcp://[::1]:2375"));
    }

    #[test]
    fn remote_docker_host_detects_remote_or_unknown_hosts() {
        assert!(is_remote_docker_host("tcp://docker.example.com:2375"));
        assert!(is_remote_docker_host("ssh://docker.example.com"));
        assert!(is_remote_docker_host("http://localhost:2375"));
    }

    #[test]
    fn print_daemon_ports_handles_multiple_rows() {
        let ports = vec![
            cella_protocol::ForwardedPortDetail {
                container_name: "app".to_string(),
                container_port: 3000,
                host_port: 49152,
                protocol: cella_protocol::PortProtocol::Tcp,
                process: Some("node".to_string()),
                url: "localhost:49152".to_string(),
                hostname: None,
            },
            cella_protocol::ForwardedPortDetail {
                container_name: "api".to_string(),
                container_port: 8080,
                host_port: 49153,
                protocol: cella_protocol::PortProtocol::Tcp,
                process: None,
                url: "localhost:49153".to_string(),
                hostname: None,
            },
        ];

        print_daemon_ports(&ports);
    }

    #[test]
    fn print_backend_ports_non_orbstack_handles_hostless_and_bound_ports() {
        let containers = vec![container(
            "cella-project",
            Some("/workspaces/project"),
            vec![port(3000, Some(49152), "tcp"), port(9229, None, "tcp")],
        )];

        print_all_container_ports(&containers, false);
        print_container_ports(&containers, false);
    }

    #[test]
    fn print_backend_ports_orbstack_uses_container_local_urls() {
        let containers = vec![container(
            "cella-project",
            Some("/workspaces/project"),
            vec![port(3000, Some(49152), "tcp")],
        )];

        print_all_container_ports(&containers, true);
        print_container_ports(&containers, true);
    }

    #[test]
    fn print_all_container_ports_falls_back_to_container_name_without_workspace_label() {
        let containers = vec![container(
            "unnamed-workspace",
            None,
            vec![port(8080, Some(49153), "tcp")],
        )];

        print_all_container_ports(&containers, false);
    }
}
