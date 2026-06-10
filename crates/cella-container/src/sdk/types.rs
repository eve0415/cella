//! Typed structs for JSON output from the Apple `container` CLI.
//!
//! Shapes follow the stable 1.0.0 output format (`ManagedResource`-conformant
//! `{id, configuration, status}` objects). All optional fields use
//! `#[serde(default)]` so missing keys are silently skipped rather than
//! causing parse failures; unknown keys are ignored for forward
//! compatibility.

use std::collections::HashMap;

use serde::Deserialize;

/// Entry returned by `container ls --format json --all`.
///
/// `container inspect` emits an array of the same shape.
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
    /// One of `"running"`, `"stopped"`, `"stopping"`, `"unknown"`.
    #[serde(default)]
    pub state: Option<String>,
    /// Live network attachments with assigned addresses (populated while
    /// the container runs).
    #[serde(default)]
    pub networks: Vec<NetworkAttachment>,
}

/// A live network attachment on a running container.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAttachment {
    /// Name of the attached network (e.g. `"default"`).
    #[serde(default)]
    pub network: Option<String>,
    /// Hostname assigned to the container on this network.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Interface address in CIDR form (e.g. `"192.168.64.2/24"`).
    #[serde(default)]
    pub ipv4_address: Option<String>,
    /// Gateway address (e.g. `"192.168.64.1"`).
    #[serde(default)]
    pub ipv4_gateway: Option<String>,
}

impl NetworkAttachment {
    /// The bare IPv4 address with any CIDR prefix length stripped.
    #[must_use]
    pub fn ipv4(&self) -> Option<&str> {
        self.ipv4_address
            .as_deref()
            .map(|cidr| cidr.split('/').next().unwrap_or(cidr))
    }
}

/// Container configuration / metadata.
///
/// Containers have no separate name field — the ID doubles as the name.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfiguration {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub image: Option<ImageDescription>,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub published_ports: Option<Vec<PublishedPort>>,
    #[serde(default)]
    pub mounts: Option<Vec<MountEntry>>,
    /// Networks the container was created with (configured attachments;
    /// see [`ContainerStatus::networks`] for live address assignments).
    #[serde(default)]
    pub networks: Vec<AttachmentConfiguration>,
}

/// Image reference recorded on a container's configuration.
#[derive(Debug, Deserialize)]
pub struct ImageDescription {
    #[serde(default)]
    pub reference: Option<String>,
}

/// A configured (create-time) network attachment.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentConfiguration {
    /// Name of the network to attach to.
    #[serde(default)]
    pub network: Option<String>,
}

/// A port mapping exposed by the container.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedPort {
    #[serde(default)]
    pub container_port: Option<u16>,
    #[serde(default)]
    pub host_port: Option<u16>,
    /// `"tcp"` or `"udp"`.
    #[serde(default)]
    pub proto: Option<String>,
}

/// A mount/volume attached to the container.
#[derive(Debug, Deserialize)]
pub struct MountEntry {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    /// Filesystem type. Swift encodes the `FSType` enum as an
    /// externally-tagged object (e.g. `{"virtiofs": {}}`).
    #[serde(default, rename = "type")]
    pub fs_type: Option<FilesystemType>,
    /// Mount options (e.g. `["ro"]`).
    #[serde(default)]
    pub options: Vec<String>,
}

/// Filesystem attachment type for a mount.
///
/// Mirrors the Swift `Filesystem.FSType` enum, whose Codable synthesis
/// encodes each case as `{"caseName": {associated values}}` — cases without
/// useful payload carry an ignored object.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilesystemType {
    /// Disk-image backed filesystem.
    Block(serde_json::Value),
    /// Named (or anonymous) managed volume.
    Volume {
        #[serde(default)]
        name: Option<String>,
    },
    /// Host directory shared via virtiofs — the bind-mount mechanism.
    Virtiofs(serde_json::Value),
    /// In-memory filesystem.
    Tmpfs(serde_json::Value),
}

impl FilesystemType {
    /// Docker-parity mount-type string (`virtiofs` surfaces as `"bind"`).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Block(_) => "block",
            Self::Volume { .. } => "volume",
            Self::Virtiofs(_) => "bind",
            Self::Tmpfs(_) => "tmpfs",
        }
    }

    /// The volume name for [`FilesystemType::Volume`] mounts.
    #[must_use]
    pub fn volume_name(&self) -> Option<&str> {
        match self {
            Self::Volume { name } => name.as_deref(),
            _ => None,
        }
    }
}

/// `container inspect <id>` returns an array of the same entries as
/// `container ls`.
pub type ContainerInspect = ContainerListEntry;

/// Entry returned by `container network ls --format json` and
/// `container network inspect`.
#[derive(Debug, Deserialize)]
pub struct NetworkListEntry {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub configuration: Option<NetworkConfiguration>,
}

/// Persistent configuration of a network.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfiguration {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    /// Creation timestamp as emitted by the CLI.
    #[serde(default)]
    pub creation_date: Option<serde_json::Value>,
}

impl NetworkListEntry {
    /// The network name (`id` and `configuration.name` are identical; either
    /// may be missing in a degraded entry).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.configuration
            .as_ref()
            .and_then(|c| c.name.as_deref())
            .or(self.id.as_deref())
    }

    /// The network's labels (empty when absent).
    #[must_use]
    pub fn labels(&self) -> HashMap<String, String> {
        self.configuration
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default()
    }
}

/// A single entry from `container system version --format json` (returns an
/// array: the CLI entry plus, when running, an API-server entry).
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
            "id": "abc123",
            "status": {
                "state": "running",
                "networks": [
                    {
                        "network": "default",
                        "hostname": "abc123",
                        "ipv4Address": "192.168.64.2/24",
                        "ipv4Gateway": "192.168.64.1",
                        "macAddress": "02:42:ac:11:00:02",
                        "mtu": 1500
                    }
                ],
                "startedDate": 771234567.0
            },
            "configuration": {
                "id": "abc123",
                "image": {
                    "reference": "docker.io/library/ubuntu:latest",
                    "descriptor": { "digest": "sha256:abc", "size": 1 }
                },
                "labels": { "dev.cella.tool": "cella" },
                "publishedPorts": [
                    {
                        "hostAddress": "0.0.0.0",
                        "containerPort": 80,
                        "hostPort": 8080,
                        "proto": "tcp",
                        "count": 1
                    }
                ],
                "networks": [ { "network": "default" } ],
                "mounts": [
                    {
                        "type": { "virtiofs": {} },
                        "source": "/host/path",
                        "destination": "/container/path",
                        "options": []
                    }
                ]
            }
        }"#;

        let entry: ContainerListEntry = serde_json::from_str(json).unwrap();
        let status = entry.status.unwrap();
        assert_eq!(status.state.as_deref(), Some("running"));
        assert_eq!(status.networks.len(), 1);
        assert_eq!(status.networks[0].ipv4(), Some("192.168.64.2"));
        assert_eq!(
            status.networks[0].ipv4_gateway.as_deref(),
            Some("192.168.64.1")
        );

        let config = entry.configuration.unwrap();
        assert_eq!(config.id.as_deref(), Some("abc123"));
        assert_eq!(
            config.image.unwrap().reference.as_deref(),
            Some("docker.io/library/ubuntu:latest")
        );

        let labels = config.labels.unwrap();
        assert_eq!(labels.get("dev.cella.tool").unwrap(), "cella");

        let ports = config.published_ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, Some(80));
        assert_eq!(ports[0].host_port, Some(8080));
        assert_eq!(ports[0].proto.as_deref(), Some("tcp"));

        assert_eq!(config.networks.len(), 1);
        assert_eq!(config.networks[0].network.as_deref(), Some("default"));

        let mounts = config.mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/host/path"));
        assert_eq!(mounts[0].fs_type.as_ref().unwrap().kind(), "bind");
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
        assert!(status.networks.is_empty());
    }

    #[test]
    fn deserialize_mount_types() {
        let json = r#"[
            { "type": { "virtiofs": {} }, "source": "/h", "destination": "/c" },
            { "type": { "volume": { "name": "data", "format": "ext4" } },
              "source": "/var/lib/volumes/data", "destination": "/data" },
            { "type": { "tmpfs": {} }, "source": "", "destination": "/tmp/x" },
            { "type": { "block": { "format": "ext4" } }, "source": "/img", "destination": "/b" }
        ]"#;
        let mounts: Vec<MountEntry> = serde_json::from_str(json).unwrap();
        let kinds: Vec<&str> = mounts
            .iter()
            .map(|m| m.fs_type.as_ref().unwrap().kind())
            .collect();
        assert_eq!(kinds, vec!["bind", "volume", "tmpfs", "block"]);
        assert_eq!(
            mounts[1].fs_type.as_ref().unwrap().volume_name(),
            Some("data")
        );
        assert_eq!(mounts[0].fs_type.as_ref().unwrap().volume_name(), None);
    }

    #[test]
    fn deserialize_mount_with_ro_option() {
        let json = r#"{
            "type": { "virtiofs": {} },
            "source": "/h",
            "destination": "/c",
            "options": ["ro"]
        }"#;
        let mount: MountEntry = serde_json::from_str(json).unwrap();
        assert_eq!(mount.options, vec!["ro"]);
    }

    #[test]
    fn deserialize_network_attachment_without_prefix() {
        let json = r#"{ "network": "cella", "ipv4Address": "192.168.65.3" }"#;
        let att: NetworkAttachment = serde_json::from_str(json).unwrap();
        assert_eq!(att.ipv4(), Some("192.168.65.3"));
    }

    #[test]
    fn deserialize_network_list_entry() {
        let json = r#"{
            "id": "cella",
            "configuration": {
                "name": "cella",
                "mode": "nat",
                "creationDate": 771234567.0,
                "labels": { "dev.cella.managed": "true" },
                "plugin": "container-network-vmnet",
                "options": {}
            },
            "status": {
                "ipv4Subnet": "192.168.65.0/24",
                "ipv4Gateway": "192.168.65.1"
            }
        }"#;
        let entry: NetworkListEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name(), Some("cella"));
        let labels = entry.configuration.unwrap().labels.unwrap();
        assert_eq!(labels.get("dev.cella.managed").unwrap(), "true");
    }

    #[test]
    fn network_list_entry_name_falls_back_to_id() {
        let json = r#"{ "id": "cella" }"#;
        let entry: NetworkListEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name(), Some("cella"));
    }

    #[test]
    fn deserialize_version_info() {
        let json = r#"[{
            "version": "1.0.0",
            "buildType": "release",
            "commit": "abcdef0",
            "appName": "container"
        }]"#;
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
