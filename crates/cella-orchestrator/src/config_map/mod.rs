//! Map devcontainer.json config to container creation options.
//!
//! This module converts devcontainer.json configuration into
//! `CreateContainerOptions` (backend-agnostic types from `cella-backend`).

pub mod env;
pub mod mounts;
pub mod ports;
pub mod run_args;

use std::collections::HashMap;
use std::path::Path;

use cella_backend::{CreateContainerOptions, GpuRequest, MountConfig, RunArgsOverrides};
use cella_features::FeatureContainerConfig;

use env::{map_container_env, map_remote_env};
use mounts::{map_additional_mounts, map_workspace_mount, parse_mount_string};
use ports::map_port_bindings;

/// Parameters for mapping a devcontainer config to container creation options.
pub struct MapConfigParams<'a, S: std::hash::BuildHasher> {
    pub config: &'a serde_json::Value,
    pub container_name: &'a str,
    pub image_name: &'a str,
    pub labels: HashMap<String, String, S>,
    pub workspace_root: &'a Path,
    pub feature_config: Option<&'a FeatureContainerConfig>,
    pub image_env: &'a [String],
    pub agent_arch: &'a str,
}

/// Map a resolved devcontainer config to container creation options.
pub fn map_config<S: std::hash::BuildHasher>(
    params: MapConfigParams<'_, S>,
) -> CreateContainerOptions {
    let MapConfigParams {
        config,
        container_name,
        image_name,
        labels,
        workspace_root,
        feature_config,
        image_env,
        agent_arch,
    } = params;
    let workspace_basename = workspace_root.file_name().map_or_else(
        || "workspace".to_string(),
        |n| n.to_string_lossy().to_string(),
    );

    let workspace_folder = config
        .get("workspaceFolder")
        .and_then(|v| v.as_str())
        .map_or_else(|| format!("/workspaces/{workspace_basename}"), String::from);

    let workspace_mount = map_workspace_mount(config, workspace_root, &workspace_folder);
    let mounts = map_merged_mounts(config, feature_config);
    let env = map_merged_env(config, image_env);
    let remote_env = map_remote_env(config);
    let port_bindings = map_port_bindings(config);
    let cap_add = map_merged_string_list(config, "capAdd", feature_config.map(|fc| &fc.cap_add));
    let security_opt = map_merged_string_list(
        config,
        "securityOpt",
        feature_config.map(|fc| &fc.security_opt),
    );

    let container_user = config
        .get("containerUser")
        .and_then(|v| v.as_str())
        .map(String::from);

    let (entrypoint, cmd) = build_entrypoint_cmd(config, feature_config, agent_arch);

    let privileged = config
        .get("privileged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || feature_config.is_some_and(|fc| fc.privileged);

    let gpu_request = map_gpu_device_request(config);
    let run_args_overrides = parse_run_args_from_config(config);

    CreateContainerOptions {
        name: container_name.to_string(),
        image: image_name.to_string(),
        labels: labels.into_iter().collect(),
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
        run_args_overrides,
        gpu_request,
    }
}

/// Build mounts from feature config (includes user mounts via merge) or directly from config.
fn map_merged_mounts(
    config: &serde_json::Value,
    feature_config: Option<&FeatureContainerConfig>,
) -> Vec<MountConfig> {
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
    mounts
}

/// Build container env: image env as base, user `containerEnv` overlays.
/// Feature `containerEnv` is NOT included here -- it's baked into the image
/// via Dockerfile ENV instructions.
fn map_merged_env(config: &serde_json::Value, image_env: &[String]) -> Vec<String> {
    let user_env = map_container_env(config);
    if user_env.is_empty() {
        Vec::new()
    } else {
        let mut merged = image_env.to_vec();
        merged.extend(user_env);
        merged
    }
}

/// Merge a string-array config key with optional feature values, deduplicated.
fn map_merged_string_list(
    config: &serde_json::Value,
    key: &str,
    feature_values: Option<&Vec<String>>,
) -> Vec<String> {
    let mut list = Vec::new();
    if let Some(fv) = feature_values {
        list.extend(fv.clone());
    }
    list.extend(map_string_array(config, key));
    list.sort();
    list.dedup();
    list
}

/// Build the entrypoint and cmd for the container.
fn build_entrypoint_cmd(
    config: &serde_json::Value,
    feature_config: Option<&FeatureContainerConfig>,
    _agent_arch: &str,
) -> (Option<Vec<String>>, Option<Vec<String>>) {
    use std::fmt::Write;

    let override_command = config
        .get("overrideCommand")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    if !override_command {
        return (None, None);
    }

    let mut script = String::from("echo Container started\ntrap \"exit 0\" 15\n");

    // Stable agent binary path inside containers. This is a devcontainer
    // convention, not Docker-specific, so we hardcode it here rather than
    // pulling in a backend-specific crate.
    let agent_path = "/cella/bin/cella-agent";
    // Restart loop: if the agent crashes, it is restarted after 1 second.
    // `restart_agent_in_container()` sends `pkill -f 'cella-agent daemon'`
    // which terminates the daemon process; the loop survives and restarts it.
    let _ = write!(
        script,
        "if [ -x \"{agent_path}\" ]; then\n  \
         while true; do \
         \"{agent_path}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" 2>/dev/null; \
         sleep 1; done &\n\
         fi\n"
    );

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
}

/// Map `hostRequirements.gpu` to a `GpuRequest` (runArgs `--gpus` takes precedence).
fn map_gpu_device_request(config: &serde_json::Value) -> Option<GpuRequest> {
    config
        .get("hostRequirements")
        .and_then(|h| h.get("gpu"))
        .and_then(|gpu| match gpu {
            serde_json::Value::Bool(true) => Some(GpuRequest::All),
            serde_json::Value::String(s) if s == "optional" => Some(GpuRequest::All),
            serde_json::Value::Object(obj) => {
                let count = obj
                    .get("cores")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(-1);
                Some(GpuRequest::Count(count))
            }
            _ => None,
        })
}

/// Parse `runArgs` from config if present.
fn parse_run_args_from_config(config: &serde_json::Value) -> Option<RunArgsOverrides> {
    config.get("runArgs").and_then(|v| v.as_array()).map(|arr| {
        let args: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let overrides = run_args::parse_run_args(&args);
        run_args::warn_unrecognized(&overrides);
        overrides
    })
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

    fn test_map_config(
        config: &serde_json::Value,
        feature_config: Option<&FeatureContainerConfig>,
        image_env: &[String],
    ) -> CreateContainerOptions {
        map_config(MapConfigParams {
            config,
            container_name: "test",
            image_name: "ubuntu",
            labels: HashMap::new(),
            workspace_root: Path::new("/tmp/test"),
            feature_config,
            image_env,
            agent_arch: "x86_64",
        })
    }

    #[test]
    fn map_image_config() {
        let config = json!({
            "image": "ubuntu",
            "remoteUser": "vscode",
        });

        let opts = map_config(MapConfigParams {
            config: &config,
            container_name: "test-container",
            image_name: "ubuntu",
            labels: HashMap::new(),
            workspace_root: Path::new("/tmp/my-project"),
            feature_config: None,
            image_env: &[],
            agent_arch: "x86_64",
        });

        assert_eq!(opts.image, "ubuntu");
        assert_eq!(opts.workspace_folder, "/workspaces/my-project");
    }

    #[test]
    fn map_workspace_folder_override() {
        let config = json!({
            "image": "ubuntu",
            "workspaceFolder": "/home/user/project",
        });

        let opts = map_config(MapConfigParams {
            config: &config,
            container_name: "test",
            image_name: "ubuntu",
            labels: HashMap::new(),
            workspace_root: Path::new("/tmp/my-project"),
            feature_config: None,
            image_env: &[],
            agent_arch: "x86_64",
        });

        assert_eq!(opts.workspace_folder, "/home/user/project");
    }

    #[test]
    fn map_container_env_values() {
        let config = json!({
            "image": "ubuntu",
            "containerEnv": {"FOO": "bar", "BAZ": "qux"},
        });

        let opts = test_map_config(&config, None, &[]);

        assert!(opts.env.contains(&"FOO=bar".to_string()));
        assert!(opts.env.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn map_override_command_default_true() {
        let config = json!({"image": "ubuntu"});
        let opts = test_map_config(&config, None, &[]);
        assert_eq!(opts.entrypoint, Some(vec!["/bin/sh".to_string()]));
        let cmd = opts.cmd.unwrap();
        assert_eq!(cmd[0], "-c");
        assert!(cmd[1].contains("while sleep 1 & wait $!; do :; done"));
        assert_eq!(cmd[2], "-");
    }

    #[test]
    fn map_override_command_false() {
        let config = json!({"image": "ubuntu", "overrideCommand": false});
        let opts = test_map_config(&config, None, &[]);
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

        let opts = test_map_config(&config, None, &[]);
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

        let opts = test_map_config(&config, None, &[]);
        assert!(opts.port_bindings.contains_key("3000/udp"));
    }

    #[test]
    fn map_mounts_object_format() {
        let config = json!({
            "image": "ubuntu",
            "mounts": [{"type": "bind", "source": "/a", "target": "/b"}],
        });

        let opts = test_map_config(&config, None, &[]);
        assert_eq!(opts.mounts.len(), 1);
        assert_eq!(opts.mounts[0].target, "/b");
    }

    #[test]
    fn map_default_workspace_mount() {
        let config = json!({"image": "ubuntu"});
        let opts = map_config(MapConfigParams {
            config: &config,
            container_name: "test",
            image_name: "ubuntu",
            labels: HashMap::new(),
            workspace_root: Path::new("/tmp/my-project"),
            feature_config: None,
            image_env: &[],
            agent_arch: "x86_64",
        });
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
        let _ = test_map_config(&config, None, &[]);
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

        let opts = test_map_config(&config, Some(&feature_config), &[]);

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

        // privileged OR'd -- feature is true
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

        let opts = test_map_config(&config, Some(&feature_config), &[]);

        // Each mount target appears exactly once
        assert_eq!(opts.mounts.len(), 2);
        assert_eq!(opts.mounts[0].target, "/feat-dst");
        assert_eq!(opts.mounts[1].target, "/container");
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

        let opts = test_map_config(&config, Some(&feature_config), &[]);

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

        let opts = test_map_config(&config, None, &image_env);

        // Image env preserved as base, user env appended
        assert!(opts.env.contains(&"PATH=/usr/bin:/bin".to_string()));
        assert!(opts.env.contains(&"HOME=/root".to_string()));
        assert!(opts.env.contains(&"FOO=bar".to_string()));
        assert_eq!(opts.env.len(), 3);
    }
}
