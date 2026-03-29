//! Map devcontainer.json config to Docker API types.

pub mod env;
mod mounts;
pub mod ports;
pub mod run_args;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use bollard::models::{
    ContainerCreateBody, DeviceMapping, DeviceRequest, HostConfig, Mount, MountTypeEnum, PortMap,
    ResourcesUlimits, RestartPolicy, RestartPolicyNameEnum,
};
pub use cella_backend::{CreateContainerOptions, GpuRequest, MountConfig, RunArgsOverrides};
use cella_features::FeatureContainerConfig;

use env::{map_container_env, map_remote_env};
use mounts::{map_additional_mounts, map_workspace_mount, parse_mount_string};
use ports::map_port_bindings;

/// Merged overrides applied during `to_bollard_config`.
struct MergedOverrides {
    labels: HashMap<String, String>,
    hostname: Option<String>,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    privileged: bool,
}

/// Convert `CreateContainerOptions` (from `cella-backend`) to a bollard
/// `ContainerCreateBody`.
///
/// This is a free function because `CreateContainerOptions` is defined in
/// `cella-backend` and we cannot add inherent methods from another crate.
pub fn to_bollard_config(opts: &CreateContainerOptions) -> ContainerCreateBody {
    let (exposed_ports, port_bindings) = build_port_mappings(opts);
    let mounts = build_deduped_mounts(opts);
    let mut host_config = build_host_config(mounts, port_bindings);
    let merged = apply_overrides(opts, &mut host_config);

    host_config.cap_add = if merged.cap_add.is_empty() {
        None
    } else {
        Some(merged.cap_add)
    };
    host_config.security_opt = if merged.security_opt.is_empty() {
        None
    } else {
        Some(merged.security_opt)
    };
    host_config.privileged = Some(merged.privileged);

    ContainerCreateBody {
        image: Some(opts.image.clone()),
        labels: Some(merged.labels),
        env: if opts.env.is_empty() {
            None
        } else {
            Some(opts.env.clone())
        },
        user: opts.user.clone(),
        hostname: merged.hostname,
        working_dir: Some(opts.workspace_folder.clone()),
        entrypoint: opts.entrypoint.clone(),
        cmd: opts.cmd.clone(),
        exposed_ports: if exposed_ports.is_empty() {
            None
        } else {
            Some(exposed_ports)
        },
        host_config: Some(host_config),
        ..Default::default()
    }
}

/// Build exposed ports and port bindings, converting `PortForward` to
/// bollard `PortBinding`.
fn build_port_mappings(opts: &CreateContainerOptions) -> (Vec<String>, PortMap) {
    let mut exposed_ports: Vec<String> = Vec::new();
    let mut port_bindings: PortMap = HashMap::new();

    for (container_port, bindings) in &opts.port_bindings {
        let port_key = if container_port.contains('/') {
            container_port.clone()
        } else {
            format!("{container_port}/tcp")
        };
        exposed_ports.push(port_key.clone());

        let bollard_bindings: Vec<bollard::models::PortBinding> = bindings
            .iter()
            .map(|pf| bollard::models::PortBinding {
                host_ip: pf.host_ip.clone(),
                host_port: pf.host_port.clone(),
            })
            .collect();
        port_bindings.insert(port_key, Some(bollard_bindings));
    }

    (exposed_ports, port_bindings)
}

/// Build deduplicated mount list (last occurrence wins per target path).
fn build_deduped_mounts(opts: &CreateContainerOptions) -> Vec<Mount> {
    let mut mounts: Vec<Mount> = Vec::new();
    if let Some(ws_mount) = &opts.workspace_mount {
        mounts.push(to_bollard_mount(ws_mount));
    }
    for m in &opts.mounts {
        mounts.push(to_bollard_mount(m));
    }

    let mut seen = HashSet::new();
    mounts.reverse();
    mounts.retain(|m| m.target.as_ref().is_none_or(|t| seen.insert(t.clone())));
    mounts.reverse();
    mounts
}

/// Apply `run_args` overrides and GPU device requests to host config.
fn apply_overrides(opts: &CreateContainerOptions, host_config: &mut HostConfig) -> MergedOverrides {
    let mut extra_hosts = vec!["host.docker.internal:host-gateway".to_string()];
    let mut security_opt = opts.security_opt.clone();
    let mut privileged = opts.privileged;
    let cap_add = opts.cap_add.clone();
    let mut labels = opts.labels.clone();
    let mut hostname = None;

    if let Some(ref ra) = opts.run_args_overrides {
        apply_run_args_to_host_config(host_config, ra);
        extra_hosts.extend(ra.extra_hosts.clone());
        security_opt.extend(ra.security_opt.clone());
        if let Some(p) = ra.privileged {
            privileged = p || privileged;
        }
        for (k, v) in &ra.labels {
            labels.insert(k.clone(), v.clone());
        }
        hostname.clone_from(&ra.hostname);
        let _ = &ra.mac_address;

        if let Some(ref gpu) = ra.gpus {
            host_config
                .device_requests
                .get_or_insert_with(Vec::new)
                .push(gpu_request_to_device_request(gpu));
        }
    }

    let has_run_args_gpu = opts
        .run_args_overrides
        .as_ref()
        .is_some_and(|ra| ra.gpus.is_some());
    if !has_run_args_gpu && let Some(ref gpu) = opts.gpu_request {
        host_config
            .device_requests
            .get_or_insert_with(Vec::new)
            .push(gpu_request_to_device_request(gpu));
    }

    host_config.extra_hosts = Some(extra_hosts);

    MergedOverrides {
        labels,
        hostname,
        cap_add,
        security_opt,
        privileged,
    }
}

/// Build initial `HostConfig` with mounts and port bindings.
fn build_host_config(mounts: Vec<Mount>, port_bindings: PortMap) -> HostConfig {
    HostConfig {
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
        ..Default::default()
    }
}

/// Apply `RunArgsOverrides` to a bollard `HostConfig`.
fn apply_run_args_to_host_config(hc: &mut HostConfig, ra: &RunArgsOverrides) {
    apply_network_overrides(hc, ra);
    apply_resource_overrides(hc, ra);
    apply_security_overrides(hc, ra);
    apply_device_overrides(hc, ra);
    apply_misc_overrides(hc, ra);
}

/// Apply networking-related overrides to `HostConfig`.
fn apply_network_overrides(hc: &mut HostConfig, ra: &RunArgsOverrides) {
    if let Some(ref v) = ra.network_mode {
        hc.network_mode = Some(v.clone());
    }
    if !ra.dns.is_empty() {
        hc.dns = Some(ra.dns.clone());
    }
    if !ra.dns_search.is_empty() {
        hc.dns_search = Some(ra.dns_search.clone());
    }
}

/// Apply resource-related overrides to `HostConfig`.
fn apply_resource_overrides(hc: &mut HostConfig, ra: &RunArgsOverrides) {
    if let Some(v) = ra.memory {
        hc.memory = Some(v);
    }
    if let Some(v) = ra.memory_swap {
        hc.memory_swap = Some(v);
    }
    if let Some(v) = ra.memory_reservation {
        hc.memory_reservation = Some(v);
    }
    if let Some(v) = ra.nano_cpus {
        hc.nano_cpus = Some(v);
    }
    if let Some(v) = ra.cpu_shares {
        hc.cpu_shares = Some(v);
    }
    if let Some(v) = ra.cpu_period {
        hc.cpu_period = Some(v);
    }
    if let Some(v) = ra.cpu_quota {
        hc.cpu_quota = Some(v);
    }
    if let Some(ref v) = ra.cpuset_cpus {
        hc.cpuset_cpus = Some(v.clone());
    }
    if let Some(ref v) = ra.cpuset_mems {
        hc.cpuset_mems = Some(v.clone());
    }
    if let Some(v) = ra.shm_size {
        hc.shm_size = Some(v);
    }
    if let Some(v) = ra.pids_limit {
        hc.pids_limit = Some(v);
    }
}

/// Apply security-related overrides to `HostConfig`.
fn apply_security_overrides(hc: &mut HostConfig, ra: &RunArgsOverrides) {
    if let Some(ref v) = ra.userns_mode {
        hc.userns_mode = Some(v.clone());
    }
    if let Some(ref v) = ra.cgroup_parent {
        hc.cgroup_parent = Some(v.clone());
    }
    if let Some(ref v) = ra.cgroupns_mode {
        hc.cgroupns_mode = Some(match v.as_str() {
            "host" => bollard::models::HostConfigCgroupnsModeEnum::HOST,
            _ => bollard::models::HostConfigCgroupnsModeEnum::PRIVATE,
        });
    }
}

/// Apply device-related overrides to `HostConfig`.
fn apply_device_overrides(hc: &mut HostConfig, ra: &RunArgsOverrides) {
    if !ra.devices.is_empty() {
        let devs = ra
            .devices
            .iter()
            .map(|d| DeviceMapping {
                path_on_host: Some(d.path_on_host.clone()),
                path_in_container: Some(d.path_in_container.clone()),
                cgroup_permissions: Some(d.cgroup_permissions.clone()),
            })
            .collect();
        hc.devices = Some(devs);
    }
    if !ra.device_cgroup_rules.is_empty() {
        hc.device_cgroup_rules = Some(ra.device_cgroup_rules.clone());
    }
}

/// Apply miscellaneous overrides (ulimits, sysctls, logging, restart, etc.).
fn apply_misc_overrides(hc: &mut HostConfig, ra: &RunArgsOverrides) {
    if !ra.ulimits.is_empty() {
        let ulimits = ra
            .ulimits
            .iter()
            .map(|u| ResourcesUlimits {
                name: Some(u.name.clone()),
                soft: Some(u.soft),
                hard: Some(u.hard),
            })
            .collect();
        hc.ulimits = Some(ulimits);
    }
    if !ra.sysctls.is_empty() {
        hc.sysctls = Some(ra.sysctls.clone());
    }
    if !ra.tmpfs.is_empty() {
        hc.tmpfs = Some(ra.tmpfs.clone());
    }
    if let Some(ref v) = ra.pid_mode {
        hc.pid_mode = Some(v.clone());
    }
    if let Some(ref v) = ra.ipc_mode {
        hc.ipc_mode = Some(v.clone());
    }
    if let Some(ref v) = ra.uts_mode {
        hc.uts_mode = Some(v.clone());
    }
    if let Some(ref v) = ra.runtime {
        hc.runtime = Some(v.clone());
    }
    if !ra.storage_opt.is_empty() {
        hc.storage_opt = Some(ra.storage_opt.clone());
    }
    if let Some(ref v) = ra.log_driver {
        let log_config = bollard::models::HostConfigLogConfig {
            typ: Some(v.clone()),
            config: if ra.log_opt.is_empty() {
                None
            } else {
                Some(ra.log_opt.clone())
            },
        };
        hc.log_config = Some(log_config);
    } else if !ra.log_opt.is_empty() {
        let log_config = bollard::models::HostConfigLogConfig {
            typ: None,
            config: Some(ra.log_opt.clone()),
        };
        hc.log_config = Some(log_config);
    }
    if let Some(ref v) = ra.restart_policy {
        let (name, max) = v.strip_prefix("on-failure:").map_or_else(
            || {
                let name = match v.as_str() {
                    "always" => RestartPolicyNameEnum::ALWAYS,
                    "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
                    "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
                    _ => RestartPolicyNameEnum::EMPTY,
                };
                (name, None)
            },
            |count| (RestartPolicyNameEnum::ON_FAILURE, count.parse::<i64>().ok()),
        );
        hc.restart_policy = Some(RestartPolicy {
            name: Some(name),
            maximum_retry_count: max,
        });
    }
    if let Some(v) = ra.init {
        hc.init = Some(v);
    }
}

/// Convert a `GpuRequest` to a bollard `DeviceRequest`.
fn gpu_request_to_device_request(gpu: &GpuRequest) -> DeviceRequest {
    let capabilities = Some(vec![vec!["gpu".to_string()]]);
    match gpu {
        GpuRequest::All => DeviceRequest {
            count: Some(-1),
            capabilities,
            ..Default::default()
        },
        GpuRequest::Count(n) => DeviceRequest {
            count: Some(*n),
            capabilities,
            ..Default::default()
        },
        GpuRequest::DeviceIds(ids) => DeviceRequest {
            device_ids: Some(ids.clone()),
            capabilities,
            ..Default::default()
        },
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
    agent_arch: &str,
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

    let version = env!("CARGO_PKG_VERSION");
    let agent_path = crate::volume::agent_binary_path(version, agent_arch);
    let _ = write!(
        script,
        "if [ -x \"{agent_path}\" ]; then\n  \
         \"{agent_path}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" &\n\
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

        let opts = test_map_config(&config, Some(&feature_config), &[]);

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
            run_args_overrides: None,
            gpu_request: None,
        };

        let bollard_config = to_bollard_config(&opts);
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
            run_args_overrides: None,
            gpu_request: None,
        };

        let bollard_config = to_bollard_config(&opts);
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

        let opts = test_map_config(&config, Some(&feature_config), &[]);

        // Feature containerEnv must NOT appear in runtime env
        assert!(opts.env.is_empty());
    }

    #[test]
    fn host_docker_internal_injected() {
        let opts = CreateContainerOptions {
            name: "test".to_string(),
            image: "ubuntu".to_string(),
            labels: HashMap::new(),
            env: Vec::new(),
            remote_env: Vec::new(),
            user: None,
            workspace_folder: "/workspace".to_string(),
            workspace_mount: None,
            mounts: Vec::new(),
            port_bindings: HashMap::new(),
            entrypoint: None,
            cmd: None,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            privileged: false,
            run_args_overrides: None,
            gpu_request: None,
        };
        let bollard_config = to_bollard_config(&opts);
        let extra_hosts = bollard_config.host_config.unwrap().extra_hosts.unwrap();
        assert!(extra_hosts.contains(&"host.docker.internal:host-gateway".to_string()));
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
