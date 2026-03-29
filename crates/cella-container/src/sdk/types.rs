//! Typed structs for JSON output from the Apple `container` CLI.
//!
//! The CLI output format is pre-1.0 and subject to change.
//! All optional fields use `#[serde(default)]` so missing keys
//! are silently skipped rather than causing parse failures.

use std::collections::HashMap;

use serde::Deserialize;

/// Entry returned by `container ls --format json --all`.
#[derive(Debug, Deserialize)]
pub struct ContainerListEntry {
    #[serde(default)]
    pub status: Option<ContainerStatus>,
    #[serde(default)]
    pub configuration: Option<ContainerConfiguration>,
}

/// Runtime status of a container.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatus {
    /// One of `"running"`, `"stopped"`, `"created"`, etc.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i64>,
}

/// Container configuration / metadata.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfiguration {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub published_ports: Option<Vec<PublishedPort>>,
    #[serde(default)]
    pub mounts: Option<Vec<MountEntry>>,
}

/// A port mapping exposed by the container.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedPort {
    #[serde(default)]
    pub container_port: Option<u16>,
    #[serde(default)]
    pub host_port: Option<u16>,
    #[serde(default)]
    pub protocol: Option<String>,
}

/// A mount/volume attached to the container.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountEntry {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default, rename = "type")]
    pub mount_type: Option<String>,
}

/// `container inspect <id>` returns the same structure as a list entry
/// but may include additional detail.
pub type ContainerInspect = ContainerListEntry;

/// Entry returned by `container image ls --format json`.
#[derive(Debug, Deserialize)]
pub struct ImageListEntry {
    #[serde(default)]
    pub reference: Option<String>,
}

/// A single entry from `container version --format json` (returns an array).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub app_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_container_list_entry() {
        let json = r#"{
            "status": { "state": "running", "exitCode": 0 },
            "configuration": {
                "id": "abc123",
                "name": "test-container",
                "image": "ubuntu:latest",
                "labels": { "dev.cella.tool": "cella" },
                "publishedPorts": [
                    { "containerPort": 8080, "hostPort": 8080, "protocol": "tcp" }
                ],
                "mounts": [
                    { "source": "/host/path", "destination": "/container/path", "type": "bind" }
                ]
            }
        }"#;

        let entry: ContainerListEntry = serde_json::from_str(json).unwrap();
        let status = entry.status.unwrap();
        assert_eq!(status.state.as_deref(), Some("running"));
        assert_eq!(status.exit_code, Some(0));

        let config = entry.configuration.unwrap();
        assert_eq!(config.id.as_deref(), Some("abc123"));
        assert_eq!(config.name.as_deref(), Some("test-container"));
        assert_eq!(config.image.as_deref(), Some("ubuntu:latest"));

        let labels = config.labels.unwrap();
        assert_eq!(labels.get("dev.cella.tool").unwrap(), "cella");

        let ports = config.published_ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, Some(8080));

        let mounts = config.mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/host/path"));
    }

    #[test]
    fn deserialize_minimal_container_entry() {
        let json = r"{}";
        let entry: ContainerListEntry = serde_json::from_str(json).unwrap();
        assert!(entry.status.is_none());
        assert!(entry.configuration.is_none());
    }

    #[test]
    fn deserialize_partial_status() {
        let json = r#"{ "status": { "state": "stopped" } }"#;
        let entry: ContainerListEntry = serde_json::from_str(json).unwrap();
        let status = entry.status.unwrap();
        assert_eq!(status.state.as_deref(), Some("stopped"));
        assert!(status.exit_code.is_none());
    }

    #[test]
    fn deserialize_image_list_entry() {
        let json = r#"{ "reference": "ubuntu:latest" }"#;
        let entry: ImageListEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.reference.as_deref(), Some("ubuntu:latest"));
    }

    #[test]
    fn deserialize_version_info() {
        let json = r#"[{ "version": "1.0.0", "appName": "container" }]"#;
        let entries: Vec<VersionInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version.as_deref(), Some("1.0.0"));
        assert_eq!(entries[0].app_name.as_deref(), Some("container"));
    }

    #[test]
    fn deserialize_version_info_missing_fields() {
        let json = r"[{}]";
        let entries: Vec<VersionInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].version.is_none());
        assert!(entries[0].app_name.is_none());
    }

    #[test]
    fn deserialize_container_list() {
        let json = r#"[
            { "status": { "state": "running" }, "configuration": { "id": "a" } },
            { "status": { "state": "stopped" }, "configuration": { "id": "b" } }
        ]"#;
        let entries: Vec<ContainerListEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{
            "status": { "state": "running", "unknownField": 42 },
            "configuration": { "id": "x", "futureField": true }
        }"#;
        let entry: ContainerListEntry = serde_json::from_str(json).unwrap();
        assert!(entry.status.is_some());
        assert!(entry.configuration.is_some());
    }
}
