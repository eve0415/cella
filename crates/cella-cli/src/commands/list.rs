use clap::Args;
use serde_json::json;

use cella_docker::{ContainerInfo, ContainerState, DockerClient};

/// List all dev containers managed by cella.
#[derive(Args)]
pub struct ListArgs {
    /// Show only running containers.
    #[arg(long)]
    running: bool,

    /// Output as JSON.
    #[arg(long)]
    json: bool,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,
}

impl ListArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };

        let containers = client.list_cella_containers(self.running).await?;

        if self.json {
            print_json(&containers);
        } else {
            print_table(&containers);
        }

        Ok(())
    }
}

const fn state_str(state: &ContainerState) -> &str {
    match state {
        ContainerState::Running => "running",
        ContainerState::Stopped => "stopped",
        ContainerState::Created => "created",
        ContainerState::Removing => "removing",
        ContainerState::Other(s) => s.as_str(),
    }
}

fn format_age(created_at: Option<&str>) -> String {
    let Some(created) = created_at else {
        return "-".to_string();
    };

    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(created) else {
        return "-".to_string();
    };

    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(dt);

    let secs = duration.num_seconds();
    if secs < 0 {
        return "-".to_string();
    }

    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn format_ports(info: &ContainerInfo) -> String {
    if info.ports.is_empty() {
        return "-".to_string();
    }
    info.ports
        .iter()
        .map(|p| {
            p.host_port.map_or_else(
                || p.container_port.to_string(),
                |hp| format!("{}:{}", hp, p.container_port),
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn short_id(id: &str) -> &str {
    &id[..12.min(id.len())]
}

fn print_table(containers: &[ContainerInfo]) {
    if containers.is_empty() {
        eprintln!("No cella containers found.");
        return;
    }

    // Print header
    println!(
        "{:<28} {:<12} {:<10} {:<40} {:<12} AGE",
        "NAME", "ID", "STATE", "WORKSPACE", "PORTS"
    );

    for c in containers {
        let workspace = c
            .labels
            .get("dev.cella.workspace_path")
            .map_or("-", String::as_str);

        println!(
            "{:<28} {:<12} {:<10} {:<40} {:<12} {}",
            c.name,
            short_id(&c.id),
            state_str(&c.state),
            workspace,
            format_ports(c),
            format_age(c.created_at.as_deref()),
        );
    }
}

fn print_json(containers: &[ContainerInfo]) {
    let items: Vec<_> = containers
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "id": short_id(&c.id),
                "state": state_str(&c.state),
                "workspace": c.labels.get("dev.cella.workspace_path").unwrap_or(&String::new()),
                "ports": format_ports(c),
                "age": format_age(c.created_at.as_deref()),
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&items).unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_age_seconds() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::seconds(30)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "30s");
    }

    #[test]
    fn test_format_age_minutes() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::minutes(5)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "5m");
    }

    #[test]
    fn test_format_age_hours() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::hours(2)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "2h");
    }

    #[test]
    fn test_format_age_days() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::days(3)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "3d");
    }

    #[test]
    fn test_format_age_none() {
        assert_eq!(format_age(None), "-");
    }

    #[test]
    fn test_format_age_invalid() {
        assert_eq!(format_age(Some("not-a-date")), "-");
    }
}
