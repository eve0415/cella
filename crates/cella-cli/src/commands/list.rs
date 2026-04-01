use std::collections::BTreeMap;

use clap::Args;
use serde_json::json;

use cella_compose::discovery;
use cella_docker::{ContainerInfo, ContainerState};

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
        let client = super::connect_docker(self.docker_host.as_deref())?;

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
    use crate::table::{Column, Table};

    if containers.is_empty() {
        eprintln!("No cella containers found.");
        return;
    }

    let mut table = Table::new(vec![
        Column::shrinkable("NAME"),
        Column::fixed("ID"),
        Column::fixed("STATE"),
        Column::fixed("BRANCH"),
        Column::shrinkable("WORKSPACE"),
        Column::fixed("PORTS"),
        Column::fixed("AGE"),
    ]);

    // Separate compose and non-compose containers
    let mut compose_projects: BTreeMap<String, Vec<&ContainerInfo>> = BTreeMap::new();
    let mut standalone: Vec<&ContainerInfo> = Vec::new();

    for c in containers {
        if let Some(project) = discovery::compose_project_from_labels(&c.labels) {
            compose_projects
                .entry(project.to_string())
                .or_default()
                .push(c);
        } else {
            standalone.push(c);
        }
    }

    // Compose projects with tree display
    for (project_name, services) in &compose_projects {
        let primary_svc = services
            .iter()
            .find(|c| discovery::is_primary_service(&c.labels));
        let workspace = primary_svc
            .and_then(|c| c.labels.get("dev.cella.workspace_path"))
            .map_or("-", String::as_str);

        table.add_row(vec![
            project_name.clone(),
            "(compose)".to_string(),
            String::new(),
            String::new(),
            workspace.to_string(),
            String::new(),
            String::new(),
        ]);

        for c in services {
            let svc_name = discovery::compose_service_from_labels(&c.labels).unwrap_or(&c.name);
            let is_primary = discovery::is_primary_service(&c.labels);
            let label = if is_primary {
                format!("  {svc_name} (primary)")
            } else {
                format!("  {svc_name}")
            };
            let branch = c.labels.get("dev.cella.branch").map_or("-", String::as_str);

            table.add_row(vec![
                label,
                short_id(&c.id).to_string(),
                state_str(&c.state).to_string(),
                branch.to_string(),
                if is_primary { workspace } else { "-" }.to_string(),
                format_ports(c),
                format_age(c.created_at.as_deref()),
            ]);
        }
    }

    // Standalone containers
    for c in &standalone {
        let workspace = c
            .labels
            .get("dev.cella.workspace_path")
            .map_or("-", String::as_str);
        let branch = c.labels.get("dev.cella.branch").map_or("-", String::as_str);

        table.add_row(vec![
            c.name.clone(),
            short_id(&c.id).to_string(),
            state_str(&c.state).to_string(),
            branch.to_string(),
            workspace.to_string(),
            format_ports(c),
            format_age(c.created_at.as_deref()),
        ]);
    }

    table.eprint();
}

fn print_json(containers: &[ContainerInfo]) {
    let items: Vec<_> = containers
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "id": short_id(&c.id),
                "state": state_str(&c.state),
                "branch": c.labels.get("dev.cella.branch").unwrap_or(&String::new()),
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

    // ── state_str ───────────────────────────────────────────────────

    #[test]
    fn state_str_running() {
        assert_eq!(state_str(&ContainerState::Running), "running");
    }

    #[test]
    fn state_str_stopped() {
        assert_eq!(state_str(&ContainerState::Stopped), "stopped");
    }

    #[test]
    fn state_str_created() {
        assert_eq!(state_str(&ContainerState::Created), "created");
    }

    #[test]
    fn state_str_removing() {
        assert_eq!(state_str(&ContainerState::Removing), "removing");
    }

    #[test]
    fn state_str_other() {
        assert_eq!(
            state_str(&ContainerState::Other("restarting".to_string())),
            "restarting"
        );
    }

    // ── short_id ────────────────────────────────────────────────────

    #[test]
    fn short_id_full_length() {
        let id = "abcdef1234567890abcdef";
        assert_eq!(short_id(id), "abcdef123456");
    }

    #[test]
    fn short_id_exact_12() {
        let id = "abcdef123456";
        assert_eq!(short_id(id), "abcdef123456");
    }

    #[test]
    fn short_id_shorter_than_12() {
        let id = "abc123";
        assert_eq!(short_id(id), "abc123");
    }

    // ── format_ports ────────────────────────────────────────────────

    fn make_container(ports: Vec<cella_docker::PortBinding>) -> ContainerInfo {
        ContainerInfo {
            id: "abc".into(),
            name: "test".into(),
            state: ContainerState::Running,
            labels: std::collections::HashMap::default(),
            ports,
            config_hash: None,
            container_user: None,
            created_at: None,
            image: None,
            mounts: vec![],
            exit_code: None,
            backend: cella_docker::BackendKind::Docker,
        }
    }

    #[test]
    fn format_ports_empty() {
        let info = make_container(vec![]);
        assert_eq!(format_ports(&info), "-");
    }

    #[test]
    fn format_ports_with_host_port() {
        let info = make_container(vec![cella_docker::PortBinding {
            container_port: 8080,
            host_port: Some(3000),
            protocol: "tcp".into(),
        }]);
        assert_eq!(format_ports(&info), "3000:8080");
    }

    #[test]
    fn format_ports_without_host_port() {
        let info = make_container(vec![cella_docker::PortBinding {
            container_port: 5432,
            host_port: None,
            protocol: "tcp".into(),
        }]);
        assert_eq!(format_ports(&info), "5432");
    }

    #[test]
    fn format_ports_multiple() {
        let info = make_container(vec![
            cella_docker::PortBinding {
                container_port: 80,
                host_port: Some(8080),
                protocol: "tcp".into(),
            },
            cella_docker::PortBinding {
                container_port: 443,
                host_port: Some(8443),
                protocol: "tcp".into(),
            },
        ]);
        assert_eq!(format_ports(&info), "8080:80,8443:443");
    }

    // ── format_age edge cases ───────────────────────────────────────

    #[test]
    fn format_age_boundary_60s_shows_minutes() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::seconds(60)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "1m");
    }

    #[test]
    fn format_age_boundary_3600s_shows_hours() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::seconds(3600)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "1h");
    }

    #[test]
    fn format_age_boundary_86400s_shows_days() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::seconds(86400)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "1d");
    }

    // ── format_age additional edge cases ───────────────────────────

    #[test]
    fn format_age_zero_seconds() {
        let now = chrono::Utc::now();
        let created = now.to_rfc3339();
        assert_eq!(format_age(Some(&created)), "0s");
    }

    #[test]
    fn format_age_59_seconds() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::seconds(59)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "59s");
    }

    #[test]
    fn format_age_large_days() {
        let now = chrono::Utc::now();
        let created = (now - chrono::Duration::days(365)).to_rfc3339();
        assert_eq!(format_age(Some(&created)), "365d");
    }

    // ── short_id edge cases ────────────────────────────────────────

    #[test]
    fn short_id_empty() {
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn short_id_single_char() {
        assert_eq!(short_id("a"), "a");
    }

    // ── format_ports edge cases ────────────────────────────────────

    #[test]
    fn format_ports_mixed_host_ports() {
        let info = make_container(vec![
            cella_docker::PortBinding {
                container_port: 80,
                host_port: Some(8080),
                protocol: "tcp".into(),
            },
            cella_docker::PortBinding {
                container_port: 443,
                host_port: None,
                protocol: "tcp".into(),
            },
        ]);
        assert_eq!(format_ports(&info), "8080:80,443");
    }

    // ── state_str completeness ─────────────────────────────────────

    #[test]
    fn state_str_other_empty() {
        assert_eq!(state_str(&ContainerState::Other(String::new())), "");
    }
}
