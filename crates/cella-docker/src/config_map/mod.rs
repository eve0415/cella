//! Map devcontainer.json config to Docker API types.

mod env;
mod mounts;
mod ports;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use bollard::container::Config;
use bollard::models::{HostConfig, Mount, MountTypeEnum, PortBinding, PortMap};
use cella_features::FeatureContainerConfig;

use env::{map_container_env, map_remote_env};
use mounts::{map_additional_mounts, map_workspace_mount, parse_mount_string};
use ports::map_port_bindings;

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

        // Deduplicate by target path — last occurrence wins (matches devcontainer CLI)
        let mut seen = HashSet::new();
        mounts.reverse();
        mounts.retain(|m| m.target.as_ref().is_none_or(|t| seen.insert(t.clone())));
        mounts.reverse();

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
            env: if self.env.is_empty() {
                None
            } else {
                Some(self.env.clone())
            },
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
    feature_config: Option<&FeatureContainerConfig>,
    image_env: &[String],
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

    // Mounts: from feature config (already includes user mounts via merge) or directly from config
    let mut mounts = Vec::new();
    if let Some(fc) = feature_config {
        for mount_str in &fc.mounts {
            if let Some(mc) = parse_mount_string(mount_str) {
                mounts.push(mc);
            }
        }
    } else {
        mounts.extend(map_additional_mounts(config));
    }

    // Build container env: image env as base, user containerEnv overlays.
    // Feature containerEnv is NOT included here — it's baked into the image
    // via Dockerfile ENV instructions (see cella-features/src/dockerfile.rs).
    let user_env = map_container_env(config);
    let env = if user_env.is_empty() {
        // No user overrides — image env preserved via None in to_bollard_config()
        Vec::new()
    } else {
        // Merge: image env first, user env last (Docker API replaces by key)
        let mut merged = image_env.to_vec();
        merged.extend(user_env);
        merged
    };

    let remote_env = map_remote_env(config);
    let port_bindings = map_port_bindings(config);

    // capAdd: feature + user, deduplicated
    let mut cap_add = Vec::new();
    if let Some(fc) = feature_config {
        cap_add.extend(fc.cap_add.clone());
    }
    cap_add.extend(map_string_array(config, "capAdd"));
    cap_add.sort();
    cap_add.dedup();

    // securityOpt: feature + user, deduplicated
    let mut security_opt = Vec::new();
    if let Some(fc) = feature_config {
        security_opt.extend(fc.security_opt.clone());
    }
    security_opt.extend(map_string_array(config, "securityOpt"));
    security_opt.sort();
    security_opt.dedup();

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
        let mut script = String::from("echo Container started\ntrap \"exit 0\" 15\n");

        if let Some(fc) = feature_config {
            for ep in &fc.entrypoints {
                script.push_str(ep);
                script.push('\n');
            }
        }

        script.push_str("exec \"$@\"\nwhile sleep 1 & wait $!; do :; done");

        (
            Some(vec!["/bin/sh".to_string()]),
            Some(vec!["-c".to_string(), script, "-".to_string()]),
        )
    } else {
        (None, None)
    };

    // privileged: OR of feature and user config
    let privileged = config
        .get("privileged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || feature_config.is_some_and(|fc| fc.privileged);

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
    use cella_features::FeatureLifecycle;
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
            None,
            &[],
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
            None,
            &[],
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
            None,
            &[],
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
            None,
            &[],
        );
        assert_eq!(opts.entrypoint, Some(vec!["/bin/sh".to_string()]));
        let cmd = opts.cmd.unwrap();
        assert_eq!(cmd[0], "-c");
        assert!(cmd[1].contains("while sleep 1 & wait $!; do :; done"));
        assert_eq!(cmd[2], "-");
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
            None,
            &[],
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
            None,
            &[],
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
            None,
            &[],
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
            None,
            &[],
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
            None,
            &[],
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
        let _ = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
            None,
            &[],
        );
    }

    #[test]
    fn map_config_merges_feature_config() {
        let config = json!({
            "image": "ubuntu",
            "containerEnv": {"USER_VAR": "user_val", "SHARED": "from_user"},
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "mounts": [{"type": "bind", "source": "/user-src", "target": "/user-dst"}],
        });

        let feature_config = FeatureContainerConfig {
            // In production, merge_with_devcontainer has already folded user mounts in
            mounts: vec![
                "source=/feat-src,target=/feat-dst".to_string(),
                "type=bind,source=/user-src,target=/user-dst".to_string(),
            ],
            cap_add: vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()],
            security_opt: vec!["apparmor=unconfined".to_string()],
            privileged: true,
            init: false,
            container_env: HashMap::from([
                ("FEAT_VAR".to_string(), "feat_val".to_string()),
                ("SHARED".to_string(), "from_feature".to_string()),
            ]),
            entrypoints: vec!["/usr/local/share/docker-init.sh".to_string()],
            lifecycle: FeatureLifecycle::default(),
            customizations: serde_json::Value::Null,
        };

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
            Some(&feature_config),
            &[],
        );

        // Feature mounts come first, then user mounts
        assert_eq!(opts.mounts.len(), 2);
        assert_eq!(opts.mounts[0].target, "/feat-dst");
        assert_eq!(opts.mounts[1].target, "/user-dst");

        // capAdd merged and deduplicated
        assert!(opts.cap_add.contains(&"SYS_PTRACE".to_string()));
        assert!(opts.cap_add.contains(&"NET_ADMIN".to_string()));
        // SYS_PTRACE appears in both but should be deduplicated
        assert_eq!(
            opts.cap_add.iter().filter(|c| *c == "SYS_PTRACE").count(),
            1
        );

        // securityOpt merged and deduplicated
        assert!(
            opts.security_opt
                .contains(&"seccomp=unconfined".to_string())
        );
        assert!(
            opts.security_opt
                .contains(&"apparmor=unconfined".to_string())
        );

        // privileged OR'd — feature is true
        assert!(opts.privileged);

        // Feature containerEnv is NOT in runtime env (baked into image via Dockerfile ENV)
        assert!(!opts.env.iter().any(|e| e.starts_with("FEAT_VAR=")));
        // User containerEnv IS present (merged with image env)
        assert!(opts.env.contains(&"USER_VAR=user_val".to_string()));
        assert!(opts.env.contains(&"SHARED=from_user".to_string()));

        // Entrypoint should be /bin/sh; feature entrypoints embedded in CMD script
        assert_eq!(opts.entrypoint, Some(vec!["/bin/sh".to_string()]));
        let cmd = opts.cmd.unwrap();
        assert_eq!(cmd[0], "-c");
        assert!(cmd[1].contains("/usr/local/share/docker-init.sh"));
        assert!(cmd[1].contains("while sleep 1 & wait $!; do :; done"));
        assert_eq!(cmd[2], "-");
    }

    #[test]
    fn feature_mounts_not_double_counted() {
        let config = json!({
            "image": "ubuntu",
            "mounts": ["source=/host,target=/container"],
        });

        // Simulate merge_with_devcontainer output: feature_config already has user mounts
        let feature_config = FeatureContainerConfig {
            mounts: vec![
                "source=/feat,target=/feat-dst".to_string(),
                "source=/host,target=/container".to_string(),
            ],
            ..Default::default()
        };

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
            Some(&feature_config),
            &[],
        );

        // Each mount target appears exactly once
        assert_eq!(opts.mounts.len(), 2);
        assert_eq!(opts.mounts[0].target, "/feat-dst");
        assert_eq!(opts.mounts[1].target, "/container");
    }

    #[test]
    fn mount_dedup_last_occurrence_wins() {
        let opts = CreateContainerOptions {
            name: "test".to_string(),
            image: "ubuntu".to_string(),
            labels: HashMap::new(),
            env: Vec::new(),
            remote_env: Vec::new(),
            user: None,
            workspace_folder: "/workspace".to_string(),
            workspace_mount: None,
            mounts: vec![
                MountConfig {
                    mount_type: "bind".to_string(),
                    source: "/first".to_string(),
                    target: "/shared-target".to_string(),
                    consistency: None,
                },
                MountConfig {
                    mount_type: "bind".to_string(),
                    source: "/second".to_string(),
                    target: "/shared-target".to_string(),
                    consistency: None,
                },
            ],
            port_bindings: HashMap::new(),
            entrypoint: None,
            cmd: None,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            privileged: false,
        };

        let bollard_config = opts.to_bollard_config();
        let mounts = bollard_config.host_config.unwrap().mounts.unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/second"));
        assert_eq!(mounts[0].target.as_deref(), Some("/shared-target"));
    }

    #[test]
    fn workspace_mount_participates_in_dedup() {
        let opts = CreateContainerOptions {
            name: "test".to_string(),
            image: "ubuntu".to_string(),
            labels: HashMap::new(),
            env: Vec::new(),
            remote_env: Vec::new(),
            user: None,
            workspace_folder: "/workspace".to_string(),
            workspace_mount: Some(MountConfig {
                mount_type: "bind".to_string(),
                source: "/ws-source".to_string(),
                target: "/workspace".to_string(),
                consistency: Some("cached".to_string()),
            }),
            mounts: vec![MountConfig {
                mount_type: "bind".to_string(),
                source: "/override-source".to_string(),
                target: "/workspace".to_string(),
                consistency: None,
            }],
            port_bindings: HashMap::new(),
            entrypoint: None,
            cmd: None,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            privileged: false,
        };

        let bollard_config = opts.to_bollard_config();
        let mounts = bollard_config.host_config.unwrap().mounts.unwrap();

        // Last occurrence wins — user mount overrides workspace mount
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/override-source"));
    }

    #[test]
    fn feature_container_env_not_in_runtime_env() {
        let config = json!({"image": "ubuntu"});
        let feature_config = FeatureContainerConfig {
            container_env: HashMap::from([
                ("NVM_DIR".to_string(), "/usr/local/share/nvm".to_string()),
                (
                    "PATH".to_string(),
                    "/usr/local/share/nvm/current/bin:${PATH}".to_string(),
                ),
            ]),
            ..Default::default()
        };

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
            Some(&feature_config),
            &[],
        );

        // Feature containerEnv must NOT appear in runtime env
        assert!(opts.env.is_empty());
    }

    #[test]
    fn user_container_env_merged_with_image_env() {
        let config = json!({
            "image": "ubuntu",
            "containerEnv": {"FOO": "bar"},
        });

        let image_env = vec!["PATH=/usr/bin:/bin".to_string(), "HOME=/root".to_string()];

        let opts = map_config(
            &config,
            "test",
            "ubuntu",
            HashMap::new(),
            Path::new("/tmp/test"),
            None,
            &image_env,
        );

        // Image env preserved as base, user env appended
        assert!(opts.env.contains(&"PATH=/usr/bin:/bin".to_string()));
        assert!(opts.env.contains(&"HOME=/root".to_string()));
        assert!(opts.env.contains(&"FOO=bar".to_string()));
        assert_eq!(opts.env.len(), 3);
    }
}
