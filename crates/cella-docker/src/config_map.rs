//! Map devcontainer.json config to Docker API types.

use std::collections::HashMap;
use std::path::Path;

use bollard::container::Config;
use bollard::models::{HostConfig, Mount, MountTypeEnum, PortBinding, PortMap};
use tracing::warn;

/// Options for creating a container (pre-mapped from devcontainer.json).
#[derive(Debug, Clone)]
pub struct CreateContainerOptions {
    pub name: String,
    pub image: String,
    pub labels: HashMap<String, String>,
    pub env: Vec<String>,
    pub remote_env: Vec<String>,
    pub user: Option<String>,
    pub workspace_folder: String,
    pub workspace_mount: Option<MountConfig>,
    pub mounts: Vec<MountConfig>,
    pub port_bindings: HashMap<String, Vec<PortBinding>>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub privileged: bool,
}

/// A mount configuration (abstracted from Docker's Mount type).
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub mount_type: String,
    pub source: String,
    pub target: String,
    pub consistency: Option<String>,
}

impl CreateContainerOptions {
    /// Convert to bollard `Config` for container creation.
    #[allow(clippy::zero_sized_map_values)] // bollard API requires HashMap<(), ()>
    pub fn to_bollard_config(&self) -> Config<String> {
        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings: PortMap = HashMap::new();

        for (container_port, bindings) in &self.port_bindings {
            let port_key = if container_port.contains('/') {
                container_port.clone()
            } else {
                format!("{container_port}/tcp")
            };
            exposed_ports.insert(port_key.clone(), HashMap::new());
            port_bindings.insert(port_key, Some(bindings.clone()));
        }

        let mut mounts: Vec<Mount> = Vec::new();
        if let Some(ws_mount) = &self.workspace_mount {
            mounts.push(to_bollard_mount(ws_mount));
        }
        for m in &self.mounts {
            mounts.push(to_bollard_mount(m));
        }

        let host_config = HostConfig {
            mounts: if mounts.is_empty() {
                None
            } else {
                Some(mounts)
            },
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            cap_add: if self.cap_add.is_empty() {
                None
            } else {
                Some(self.cap_add.clone())
            },
            security_opt: if self.security_opt.is_empty() {
                None
            } else {
                Some(self.security_opt.clone())
            },
            privileged: Some(self.privileged),
            ..Default::default()
        };

        Config {
            image: Some(self.image.clone()),
            labels: Some(self.labels.clone()),
            env: Some(self.env.clone()),
            user: self.user.clone(),
            working_dir: Some(self.workspace_folder.clone()),
            entrypoint: self.entrypoint.clone(),
            cmd: self.cmd.clone(),
            exposed_ports: if exposed_ports.is_empty() {
                None
            } else {
                Some(exposed_ports)
            },
            host_config: Some(host_config),
            ..Default::default()
        }
    }
}

fn to_bollard_mount(m: &MountConfig) -> Mount {
    Mount {
        target: Some(m.target.clone()),
        source: Some(m.source.clone()),
        typ: Some(match m.mount_type.as_str() {
            "volume" => MountTypeEnum::VOLUME,
            "tmpfs" => MountTypeEnum::TMPFS,
            _ => MountTypeEnum::BIND,
        }),
        consistency: m.consistency.clone(),
        ..Default::default()
    }
}

/// Map a resolved devcontainer config to container creation options.
#[allow(clippy::implicit_hasher)]
pub fn map_config(
    config: &serde_json::Value,
    container_name: &str,
    image_name: &str,
    labels: HashMap<String, String>,
    workspace_root: &Path,
) -> CreateContainerOptions {
    let workspace_basename = workspace_root.file_name().map_or_else(
        || "workspace".to_string(),
        |n| n.to_string_lossy().to_string(),
    );

    let workspace_folder = config
        .get("workspaceFolder")
        .and_then(|v| v.as_str())
        .map_or_else(|| format!("/workspaces/{workspace_basename}"), String::from);

    let workspace_mount = map_workspace_mount(config, workspace_root, &workspace_folder);
    let mounts = map_additional_mounts(config);
    let env = map_container_env(config);
    let remote_env = map_remote_env(config);
    let port_bindings = map_port_bindings(config);
    let cap_add = map_string_array(config, "capAdd");
    let security_opt = map_string_array(config, "securityOpt");

    let container_user = config
        .get("containerUser")
        .and_then(|v| v.as_str())
        .map(String::from);

    // overrideCommand: if false, preserve image CMD/ENTRYPOINT
    // if true or unset, override with sleep infinity
    let override_command = config
        .get("overrideCommand")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    let (entrypoint, cmd) = if override_command {
        (
            Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
            Some(vec!["while sleep 1000; do :; done".to_string()]),
        )
    } else {
        (None, None)
    };

    let privileged = config
        .get("privileged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Warn about unsupported features
    if let Some(features) = config.get("features")
        && features.as_object().is_some_and(|obj| !obj.is_empty())
    {
        warn!("OCI Features are not yet supported and will be skipped");
    }

    CreateContainerOptions {
        name: container_name.to_string(),
        image: image_name.to_string(),
        labels,
        env,
        remote_env,
        user: container_user,
        workspace_folder,
        workspace_mount,
        mounts,
        port_bindings,
        entrypoint,
        cmd,
        cap_add,
        security_opt,
        privileged,
    }
}

fn map_workspace_mount(
    config: &serde_json::Value,
    workspace_root: &Path,
    workspace_folder: &str,
) -> Option<MountConfig> {
    if let Some(mount_str) = config.get("workspaceMount").and_then(|v| v.as_str()) {
        if mount_str.is_empty() {
            return None; // Explicitly disabled
        }
        return parse_mount_string(mount_str);
    }

    // Default workspace mount
    Some(MountConfig {
        mount_type: "bind".to_string(),
        source: workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf())
            .to_string_lossy()
            .to_string(),
        target: workspace_folder.to_string(),
        consistency: Some("cached".to_string()),
    })
}

fn map_additional_mounts(config: &serde_json::Value) -> Vec<MountConfig> {
    let Some(mounts) = config.get("mounts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    mounts
        .iter()
        .filter_map(|m| match m {
            serde_json::Value::String(s) => parse_mount_string(s),
            serde_json::Value::Object(obj) => {
                let mount_type = obj
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("bind")
                    .to_string();
                let source = obj
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let target = obj
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if target.is_empty() {
                    return None;
                }

                Some(MountConfig {
                    mount_type,
                    source,
                    target,
                    consistency: None,
                })
            }
            _ => None,
        })
        .collect()
}

fn parse_mount_string(s: &str) -> Option<MountConfig> {
    let mut mount_type = "bind".to_string();
    let mut source = String::new();
    let mut target = String::new();
    let mut consistency = None;

    for part in s.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key.trim() {
                "type" => mount_type = value.to_string(),
                "source" | "src" => source = value.to_string(),
                "target" | "dst" | "destination" => target = value.to_string(),
                "consistency" => consistency = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if target.is_empty() {
        return None;
    }

    Some(MountConfig {
        mount_type,
        source,
        target,
        consistency,
    })
}

fn map_container_env(config: &serde_json::Value) -> Vec<String> {
    let Some(env_obj) = config.get("containerEnv").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    env_obj
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
        .collect()
}

fn map_remote_env(config: &serde_json::Value) -> Vec<String> {
    let Some(env_obj) = config.get("remoteEnv").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    env_obj
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
        .collect()
}

fn map_port_bindings(config: &serde_json::Value) -> HashMap<String, Vec<PortBinding>> {
    let Some(ports) = config.get("forwardPorts").and_then(|v| v.as_array()) else {
        return HashMap::new();
    };

    let ports_attrs = config.get("portsAttributes").and_then(|v| v.as_object());
    let mut bindings = HashMap::new();

    for port_value in ports {
        let port = match port_value {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => continue,
        };

        let protocol = ports_attrs
            .and_then(|attrs| attrs.get(&port))
            .and_then(|attr| attr.get("protocol"))
            .and_then(|v| v.as_str())
            .unwrap_or("tcp");

        let container_port = format!("{port}/{protocol}");
        let host_port = port.clone();

        bindings.insert(
            container_port,
            vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host_port),
            }],
        );
    }

    bindings
}

fn map_string_array(config: &serde_json::Value, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_image_config() {
        let config = json!({
            "image": "ubuntu",
            "remoteUser": "vscode",
        });

        let opts = map_config(
            &config,
            "test-container",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/my-project"),
        );

        assert_eq!(opts.image, "ubuntu");
        assert_eq!(opts.workspace_folder, "/workspaces/my-project");
    }

    #[test]
    fn map_workspace_folder_override() {
        let config = json!({
            "image": "ubuntu",
            "workspaceFolder": "/home/user/project",
        });

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/my-project"),
        );

        assert_eq!(opts.workspace_folder, "/home/user/project");
    }

    #[test]
    fn map_container_env_values() {
        let config = json!({
            "image": "ubuntu",
            "containerEnv": {"FOO": "bar", "BAZ": "qux"},
        });

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );

        assert!(opts.env.contains(&"FOO=bar".to_string()));
        assert!(opts.env.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn map_override_command_default_true() {
        let config = json!({"image": "ubuntu"});
        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );
        assert!(opts.entrypoint.is_some());
    }

    #[test]
    fn map_override_command_false() {
        let config = json!({"image": "ubuntu", "overrideCommand": false});
        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );
        assert!(opts.entrypoint.is_none());
        assert!(opts.cmd.is_none());
    }

    #[test]
    fn parse_mount_string_full() {
        let m = parse_mount_string("type=bind,source=/host,target=/container,consistency=cached")
            .unwrap();
        assert_eq!(m.mount_type, "bind");
        assert_eq!(m.source, "/host");
        assert_eq!(m.target, "/container");
        assert_eq!(m.consistency.as_deref(), Some("cached"));
    }

    #[test]
    fn parse_mount_string_minimal() {
        let m = parse_mount_string("source=/host,target=/container").unwrap();
        assert_eq!(m.mount_type, "bind");
        assert_eq!(m.source, "/host");
        assert_eq!(m.target, "/container");
    }

    #[test]
    fn parse_mount_string_no_target_returns_none() {
        assert!(parse_mount_string("source=/host").is_none());
    }

    #[test]
    fn map_forward_ports() {
        let config = json!({
            "image": "ubuntu",
            "forwardPorts": [3000, 8080],
        });

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );
        assert!(opts.port_bindings.contains_key("3000/tcp"));
        assert!(opts.port_bindings.contains_key("8080/tcp"));
    }

    #[test]
    fn map_ports_with_protocol() {
        let config = json!({
            "image": "ubuntu",
            "forwardPorts": [3000],
            "portsAttributes": {"3000": {"protocol": "udp"}},
        });

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );
        assert!(opts.port_bindings.contains_key("3000/udp"));
    }

    #[test]
    fn map_mounts_object_format() {
        let config = json!({
            "image": "ubuntu",
            "mounts": [{"type": "bind", "source": "/a", "target": "/b"}],
        });

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );
        assert_eq!(opts.mounts.len(), 1);
        assert_eq!(opts.mounts[0].target, "/b");
    }

    #[test]
    fn map_default_workspace_mount() {
        let config = json!({"image": "ubuntu"});
        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/my-project"),
        );
        assert!(opts.workspace_mount.is_some());
        let mount = opts.workspace_mount.unwrap();
        assert_eq!(mount.target, "/workspaces/my-project");
    }

    #[test]
    fn features_produces_no_panic() {
        let config = json!({
            "image": "ubuntu",
            "features": {"ghcr.io/devcontainers/features/node:1": {}},
        });
        // Just verify it doesn't panic — warning is logged
        let _ = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
        );
    }
}
