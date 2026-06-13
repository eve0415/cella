//! Helpers for building daemon container registration payloads.

use std::collections::HashMap;
use std::path::Path;

use cella_backend::{BACKEND_LABEL, ContainerInfo};
use cella_protocol::ContainerRegistrationData;

/// Build daemon registration data from a devcontainer config.
pub fn from_devcontainer_config(
    config: &serde_json::Value,
    workspace_root: &Path,
    container_id: impl Into<String>,
    container_name: impl Into<String>,
    container_ip: Option<String>,
    backend_kind: Option<String>,
    docker_host: Option<String>,
) -> ContainerRegistrationData {
    ContainerRegistrationData {
        container_id: container_id.into(),
        container_name: container_name.into(),
        container_ip,
        ports_attributes: crate::config_map::ports::parse_ports_attributes(config),
        other_ports_attributes: crate::config_map::ports::parse_other_ports_attributes(config),
        forward_ports: parse_forward_ports(config),
        shutdown_action: config
            .get("shutdownAction")
            .and_then(|v| v.as_str())
            .map(String::from),
        backend_kind,
        docker_host,
        project_name: Some(project_name_from_config(config, workspace_root)),
        branch: Some(current_branch_or_main(workspace_root)),
    }
}

/// Build daemon registration data from existing container labels.
pub fn from_container_labels(
    container: &ContainerInfo,
    container_ip: Option<String>,
    docker_host: Option<String>,
) -> ContainerRegistrationData {
    let (ports_attributes, other_ports_attributes) = container
        .labels
        .get("dev.cella.ports_attributes")
        .map(|label| crate::config_map::ports::deserialize_ports_attributes_label(label))
        .unwrap_or_default();

    ContainerRegistrationData {
        container_id: container.id.clone(),
        container_name: container.name.clone(),
        container_ip,
        ports_attributes,
        other_ports_attributes,
        forward_ports: Vec::new(),
        shutdown_action: container.labels.get("dev.cella.shutdown_action").cloned(),
        backend_kind: container.labels.get(BACKEND_LABEL).cloned(),
        docker_host,
        project_name: project_name_from_labels(&container.labels),
        branch: container.labels.get("dev.cella.branch").cloned(),
    }
}

fn project_name_from_labels(labels: &HashMap<String, String>) -> Option<String> {
    labels
        .get("dev.cella.workspace_path")
        .and_then(|p| Path::new(p).file_name())
        .map(|n| n.to_string_lossy().to_string())
}

fn project_name_from_config(config: &serde_json::Value, workspace_root: &Path) -> String {
    config
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|name| !name.trim().is_empty())
        .map_or_else(
            || {
                workspace_root.file_name().map_or_else(
                    || "workspace".to_string(),
                    |n| n.to_string_lossy().to_string(),
                )
            },
            String::from,
        )
}

fn current_branch_or_main(workspace_root: &Path) -> String {
    cella_git::discover(workspace_root)
        .ok()
        .and_then(|repo| repo.head_branch)
        .filter(|branch| !branch.trim().is_empty())
        .unwrap_or_else(|| "main".to_string())
}

fn parse_forward_ports(config: &serde_json::Value) -> Vec<u16> {
    config
        .get("forwardPorts")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(forward_port_number).collect())
        .unwrap_or_default()
}

/// Extract a `u16` port from a `forwardPorts` entry.
///
/// Accepts a JSON number or a pure-numeric string (`"9000"` is valid per the
/// spec). A `"host:port"` string (e.g. `"db:5432"`) is intentionally NOT
/// reduced to its port number here: that port lives on another container
/// reached via the workspace container's network, so registering the bare
/// number would forward the wrong target. Such entries are left for full
/// `host:port` support and ignored by this number-only path.
fn forward_port_number(value: &serde_json::Value) -> Option<u16> {
    let n = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))?;
    u16::try_from(n).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cella_backend::{BACKEND_LABEL, BackendKind, ContainerInfo, ContainerState};
    use serde_json::json;

    use super::*;

    #[test]
    fn forward_port_number_accepts_numbers_and_numeric_strings() {
        assert_eq!(forward_port_number(&json!(3000)), Some(3000));
        assert_eq!(forward_port_number(&json!("9000")), Some(9000));
        // Out of u16 range.
        assert_eq!(forward_port_number(&json!(70000)), None);
        // host:port forms are not reduced to a bare port.
        assert_eq!(forward_port_number(&json!("db:5432")), None);
        assert_eq!(forward_port_number(&json!("localhost:3000")), None);
        // Non-numeric junk.
        assert_eq!(forward_port_number(&json!("abc")), None);
    }

    #[test]
    fn devcontainer_config_registration_extracts_daemon_fields() {
        let config = json!({
            "forwardPorts": [3000, 8080, 70000, "9000", "db:5432"],
            "portsAttributes": {
                "3000": {"label": "web"}
            },
            "otherPortsAttributes": {"onAutoForward": "silent"},
            "shutdownAction": "none"
        });
        let tmp = tempfile::tempdir().unwrap();

        let data = from_devcontainer_config(
            &config,
            tmp.path(),
            "abc123",
            "cella-test",
            Some("172.20.0.5".to_string()),
            Some("docker".to_string()),
            Some("unix:///var/run/docker.sock".to_string()),
        );

        assert_eq!(data.container_id, "abc123");
        assert_eq!(data.container_name, "cella-test");
        assert_eq!(data.container_ip.as_deref(), Some("172.20.0.5"));
        // Numeric strings ("9000") are accepted; out-of-range (70000) and
        // "host:port" forms ("db:5432") are dropped by the number-only path.
        assert_eq!(data.forward_ports, vec![3000, 8080, 9000]);
        assert_eq!(data.shutdown_action.as_deref(), Some("none"));
        assert_eq!(data.backend_kind.as_deref(), Some("docker"));
        assert_eq!(
            data.docker_host.as_deref(),
            Some("unix:///var/run/docker.sock")
        );
        assert_eq!(data.ports_attributes.len(), 1);
        assert!(data.other_ports_attributes.is_some());
    }

    #[test]
    fn container_labels_registration_preserves_backend_and_shutdown() {
        let mut labels = HashMap::new();
        labels.insert(BACKEND_LABEL.to_string(), "docker".to_string());
        labels.insert(
            "dev.cella.shutdown_action".to_string(),
            "stopContainer".to_string(),
        );

        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "cella-test".to_string(),
            state: ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports: Vec::new(),
            created_at: None,
            container_user: None,
            image: Some("example:latest".to_string()),
            mounts: Vec::new(),
            backend: BackendKind::Docker,
        };

        let data = from_container_labels(
            &container,
            Some("172.20.0.5".to_string()),
            Some("unix:///var/run/docker.sock".to_string()),
        );

        assert_eq!(data.container_id, "abc123");
        assert_eq!(data.container_name, "cella-test");
        assert_eq!(data.container_ip.as_deref(), Some("172.20.0.5"));
        assert!(data.forward_ports.is_empty());
        assert_eq!(data.shutdown_action.as_deref(), Some("stopContainer"));
        assert_eq!(data.backend_kind.as_deref(), Some("docker"));
        assert_eq!(
            data.docker_host.as_deref(),
            Some("unix:///var/run/docker.sock")
        );
    }

    #[test]
    fn container_labels_derives_project_name_from_workspace_path() {
        let mut labels = HashMap::new();
        labels.insert(
            "dev.cella.workspace_path".to_string(),
            "/home/user/myapp".to_string(),
        );
        labels.insert("dev.cella.branch".to_string(), "feature/auth".to_string());

        let container = ContainerInfo {
            id: "c1".to_string(),
            name: "cella-myapp-abc123".to_string(),
            state: ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports: Vec::new(),
            created_at: None,
            container_user: None,
            image: None,
            mounts: Vec::new(),
            backend: BackendKind::Docker,
        };

        let data = from_container_labels(&container, None, None);
        assert_eq!(data.project_name.as_deref(), Some("myapp"));
        assert_eq!(data.branch.as_deref(), Some("feature/auth"));
    }

    #[test]
    fn devcontainer_config_extracts_project_name_from_name_field() {
        let config = json!({ "name": "my-project" });
        let tmp = tempfile::tempdir().unwrap();
        let data = from_devcontainer_config(&config, tmp.path(), "id", "name", None, None, None);
        assert_eq!(data.project_name.as_deref(), Some("my-project"));
        assert_eq!(data.branch.as_deref(), Some("main"));
    }

    #[test]
    fn devcontainer_config_without_name_uses_workspace_directory() {
        let config = json!({});
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("my-workspace");
        std::fs::create_dir(&workspace).unwrap();
        let data = from_devcontainer_config(&config, &workspace, "id", "name", None, None, None);
        assert_eq!(data.project_name.as_deref(), Some("my-workspace"));
        assert_eq!(data.branch.as_deref(), Some("main"));
    }

    #[test]
    fn devcontainer_config_uses_current_git_branch() {
        let config = json!({});
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            &["init"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test User"],
            &["commit", "--allow-empty", "-m", "init"],
            &["checkout", "-b", "feature/auth"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(tmp.path())
                .output()
                .unwrap();
        }

        let data = from_devcontainer_config(&config, tmp.path(), "id", "name", None, None, None);

        assert_eq!(data.branch.as_deref(), Some("feature/auth"));
    }
}
