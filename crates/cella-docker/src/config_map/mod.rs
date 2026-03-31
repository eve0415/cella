//! Convert `CreateContainerOptions` to bollard Docker API types.

pub mod env;

use std::collections::{HashMap, HashSet};

use bollard::models::{
    ContainerCreateBody, DeviceMapping, DeviceRequest, HostConfig, Mount, MountTypeEnum, PortMap,
    ResourcesUlimits, RestartPolicy, RestartPolicyNameEnum,
};
use cella_backend::{CreateContainerOptions, GpuRequest, MountConfig, RunArgsOverrides};

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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Last occurrence wins -- user mount overrides workspace mount
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("/override-source"));
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
}
