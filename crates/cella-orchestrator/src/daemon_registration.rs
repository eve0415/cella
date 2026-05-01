//! Helpers for building daemon container registration payloads.

use cella_backend::{BACKEND_LABEL, ContainerInfo};
use cella_protocol::ContainerRegistrationData;

/// Build daemon registration data from a devcontainer config.
pub fn from_devcontainer_config(
    config: &serde_json::Value,
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
        project_name: config
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from),
        branch: None,
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
        project_name: None,
        branch: container.labels.get("dev.cella.branch").cloned(),
    }
}

fn parse_forward_ports(config: &serde_json::Value) -> Vec<u16> {
    config
        .get("forwardPorts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cella_backend::{BACKEND_LABEL, BackendKind, ContainerInfo, ContainerState};
    use serde_json::json;

    use super::*;

    #[test]
    fn devcontainer_config_registration_extracts_daemon_fields() {
        let config = json!({
            "forwardPorts": [3000, 8080, 70000, "9000"],
            "portsAttributes": {
                "3000": {"label": "web"}
            },
            "otherPortsAttributes": {"onAutoForward": "silent"},
            "shutdownAction": "none"
        });

        let data = from_devcontainer_config(
            &config,
            "abc123",
            "cella-test",
            Some("172.20.0.5".to_string()),
            Some("docker".to_string()),
            Some("unix:///var/run/docker.sock".to_string()),
        );

        assert_eq!(data.container_id, "abc123");
        assert_eq!(data.container_name, "cella-test");
        assert_eq!(data.container_ip.as_deref(), Some("172.20.0.5"));
        assert_eq!(data.forward_ports, vec![3000, 8080]);
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
}
