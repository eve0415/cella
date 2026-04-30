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
    if let Some(ws_mount) = &opts.workspace_mount
        && let Some(m) = to_bollard_mount(ws_mount)
    {
        mounts.push(m);
    }
    for m in &opts.mounts {
        if let Some(bm) = to_bollard_mount(m) {
            mounts.push(bm);
        }
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

fn to_bollard_mount(m: &MountConfig) -> Option<Mount> {
    let typ = match m.mount_type.as_str() {
        "bind" => MountTypeEnum::BIND,
        "volume" => MountTypeEnum::VOLUME,
        "tmpfs" => MountTypeEnum::TMPFS,
        "npipe" => MountTypeEnum::NPIPE,
        other => {
            tracing::warn!(
                mount_type = other,
                target = %m.target,
                "unsupported mount type — skipping"
            );
            return None;
        }
    };
    Some(Mount {
        target: Some(m.target.clone()),
        source: Some(m.source.clone()),
        typ: Some(typ),
        consistency: m.consistency.clone(),
        read_only: Some(m.read_only),
        ..Default::default()
    })
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
                    read_only: false,
                    external: false,
                },
                MountConfig {
                    mount_type: "bind".to_string(),
                    source: "/second".to_string(),
                    target: "/shared-target".to_string(),
                    consistency: None,
                    read_only: false,
                    external: false,
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
                read_only: false,
                external: false,
            }),
            mounts: vec![MountConfig {
                mount_type: "bind".to_string(),
                source: "/override-source".to_string(),
                target: "/workspace".to_string(),
                consistency: None,
                read_only: false,
                external: false,
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

    /// Helper to build a minimal `CreateContainerOptions` for tests.
    fn minimal_opts() -> CreateContainerOptions {
        CreateContainerOptions {
            name: "test".to_string(),
            image: "ubuntu:latest".to_string(),
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
        }
    }

    #[test]
    fn minimal_config_sets_image_and_workspace() {
        let opts = minimal_opts();
        let config = to_bollard_config(&opts);
        assert_eq!(config.image, Some("ubuntu:latest".to_string()));
        assert_eq!(config.working_dir, Some("/workspace".to_string()));
    }

    #[test]
    fn empty_env_produces_none() {
        let opts = minimal_opts();
        let config = to_bollard_config(&opts);
        assert!(config.env.is_none());
    }

    #[test]
    fn env_vars_passed_through() {
        let mut opts = minimal_opts();
        opts.env = vec!["FOO=bar".to_string(), "BAZ=qux".to_string()];
        let config = to_bollard_config(&opts);
        assert_eq!(
            config.env,
            Some(vec!["FOO=bar".to_string(), "BAZ=qux".to_string()])
        );
    }

    #[test]
    fn user_passed_through() {
        let mut opts = minimal_opts();
        opts.user = Some("vscode".to_string());
        let config = to_bollard_config(&opts);
        assert_eq!(config.user, Some("vscode".to_string()));
    }

    #[test]
    fn entrypoint_and_cmd_passed_through() {
        let mut opts = minimal_opts();
        opts.entrypoint = Some(vec!["/bin/sh".to_string()]);
        opts.cmd = Some(vec!["-c".to_string(), "echo hi".to_string()]);
        let config = to_bollard_config(&opts);
        assert_eq!(config.entrypoint, Some(vec!["/bin/sh".to_string()]));
        assert_eq!(
            config.cmd,
            Some(vec!["-c".to_string(), "echo hi".to_string()])
        );
    }

    #[test]
    fn labels_passed_through() {
        let mut opts = minimal_opts();
        opts.labels
            .insert("dev.cella.test".to_string(), "yes".to_string());
        let config = to_bollard_config(&opts);
        let labels = config.labels.unwrap();
        assert_eq!(labels.get("dev.cella.test"), Some(&"yes".to_string()));
    }

    #[test]
    fn port_bindings_with_explicit_protocol() {
        let mut opts = minimal_opts();
        opts.port_bindings.insert(
            "8080/udp".to_string(),
            vec![cella_backend::PortForward {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("9090".to_string()),
            }],
        );
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let pb = hc.port_bindings.unwrap();
        assert!(pb.contains_key("8080/udp"));
        let exposed = config.exposed_ports.unwrap();
        assert!(exposed.contains(&"8080/udp".to_string()));
    }

    #[test]
    fn port_bindings_default_to_tcp() {
        let mut opts = minimal_opts();
        opts.port_bindings.insert(
            "3000".to_string(),
            vec![cella_backend::PortForward {
                host_ip: None,
                host_port: None,
            }],
        );
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let pb = hc.port_bindings.unwrap();
        assert!(pb.contains_key("3000/tcp"));
    }

    #[test]
    fn empty_port_bindings_produces_none() {
        let opts = minimal_opts();
        let config = to_bollard_config(&opts);
        assert!(config.exposed_ports.is_none());
        assert!(config.host_config.unwrap().port_bindings.is_none());
    }

    #[test]
    fn mount_type_volume() {
        let m = MountConfig {
            mount_type: "volume".to_string(),
            source: "my-vol".to_string(),
            target: "/data".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        };
        let bollard_mount = to_bollard_mount(&m).unwrap();
        assert_eq!(bollard_mount.typ, Some(MountTypeEnum::VOLUME));
    }

    #[test]
    fn mount_type_tmpfs() {
        let m = MountConfig {
            mount_type: "tmpfs".to_string(),
            source: String::new(),
            target: "/tmp".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        };
        let bollard_mount = to_bollard_mount(&m).unwrap();
        assert_eq!(bollard_mount.typ, Some(MountTypeEnum::TMPFS));
    }

    #[test]
    fn mount_type_bind() {
        let m = MountConfig {
            mount_type: "bind".to_string(),
            source: "/host".to_string(),
            target: "/container".to_string(),
            consistency: Some("cached".to_string()),
            read_only: false,
            external: false,
        };
        let bollard_mount = to_bollard_mount(&m).unwrap();
        assert_eq!(bollard_mount.typ, Some(MountTypeEnum::BIND));
        assert_eq!(bollard_mount.consistency, Some("cached".to_string()));
    }

    #[test]
    fn mount_type_npipe_maps_to_npipe_variant() {
        let m = MountConfig {
            mount_type: "npipe".to_string(),
            source: "//./pipe/docker_engine".to_string(),
            target: "//./pipe/docker_engine".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        };
        let bollard_mount = to_bollard_mount(&m).unwrap();
        assert_eq!(
            bollard_mount.typ,
            Some(MountTypeEnum::NPIPE),
            "npipe mount type must map to MountTypeEnum::NPIPE"
        );
    }

    #[test]
    fn mount_unknown_type_returns_none() {
        let m = MountConfig {
            mount_type: "something_else".to_string(),
            source: "/a".to_string(),
            target: "/b".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        };
        assert!(
            to_bollard_mount(&m).is_none(),
            "unsupported mount types must be rejected (return None)"
        );
    }

    #[test]
    fn mount_read_only_forwarded_to_bollard() {
        // MountConfig.read_only must be wired through to bollard Mount.read_only.
        let ro = MountConfig {
            mount_type: "bind".to_string(),
            source: "/host/data".to_string(),
            target: "/container/data".to_string(),
            consistency: None,
            read_only: true,
            external: false,
        };
        let rw = MountConfig {
            mount_type: "bind".to_string(),
            source: "/host/data".to_string(),
            target: "/container/data".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        };
        assert_eq!(
            to_bollard_mount(&ro).unwrap().read_only,
            Some(true),
            "read_only:true must reach bollard Mount"
        );
        assert_eq!(
            to_bollard_mount(&rw).unwrap().read_only,
            Some(false),
            "read_only:false must reach bollard Mount"
        );
    }

    #[test]
    fn gpu_request_all() {
        let req = GpuRequest::All;
        let dev = gpu_request_to_device_request(&req);
        assert_eq!(dev.count, Some(-1));
        assert!(dev.capabilities.is_some());
        assert_eq!(dev.capabilities.unwrap(), vec![vec!["gpu".to_string()]]);
    }

    #[test]
    fn gpu_request_count() {
        let req = GpuRequest::Count(2);
        let dev = gpu_request_to_device_request(&req);
        assert_eq!(dev.count, Some(2));
    }

    #[test]
    fn gpu_request_device_ids() {
        let req = GpuRequest::DeviceIds(vec!["0".to_string(), "1".to_string()]);
        let dev = gpu_request_to_device_request(&req);
        assert_eq!(dev.device_ids, Some(vec!["0".to_string(), "1".to_string()]));
        assert!(dev.count.is_none());
    }

    #[test]
    fn cap_add_and_security_opt() {
        let mut opts = minimal_opts();
        opts.cap_add = vec!["SYS_PTRACE".to_string()];
        opts.security_opt = vec!["seccomp=unconfined".to_string()];
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.cap_add, Some(vec!["SYS_PTRACE".to_string()]));
        assert_eq!(
            hc.security_opt,
            Some(vec!["seccomp=unconfined".to_string()])
        );
    }

    #[test]
    fn empty_cap_add_and_security_opt_produce_none() {
        let opts = minimal_opts();
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert!(hc.cap_add.is_none());
        assert!(hc.security_opt.is_none());
    }

    #[test]
    fn privileged_flag() {
        let mut opts = minimal_opts();
        opts.privileged = true;
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.privileged, Some(true));
    }

    #[test]
    fn run_args_overrides_hostname() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            hostname: Some("my-host".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        assert_eq!(config.hostname, Some("my-host".to_string()));
    }

    #[test]
    fn run_args_overrides_extra_hosts_merged() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            extra_hosts: vec!["custom:1.2.3.4".to_string()],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let extra_hosts = config.host_config.unwrap().extra_hosts.unwrap();
        assert!(extra_hosts.contains(&"host.docker.internal:host-gateway".to_string()));
        assert!(extra_hosts.contains(&"custom:1.2.3.4".to_string()));
    }

    #[test]
    fn run_args_overrides_labels_merged() {
        let mut opts = minimal_opts();
        opts.labels.insert("base".to_string(), "value".to_string());
        opts.run_args_overrides = Some(RunArgsOverrides {
            labels: std::iter::once(("override".to_string(), "yes".to_string())).collect(),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let labels = config.labels.unwrap();
        assert_eq!(labels.get("base"), Some(&"value".to_string()));
        assert_eq!(labels.get("override"), Some(&"yes".to_string()));
    }

    #[test]
    fn run_args_privileged_or_with_base() {
        let mut opts = minimal_opts();
        opts.privileged = true;
        opts.run_args_overrides = Some(RunArgsOverrides {
            privileged: Some(false),
            ..Default::default()
        });
        // privileged is OR'd: true || false = true
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.privileged, Some(true));
    }

    #[test]
    fn run_args_gpu_takes_precedence_over_gpu_request() {
        let mut opts = minimal_opts();
        opts.gpu_request = Some(GpuRequest::All);
        opts.run_args_overrides = Some(RunArgsOverrides {
            gpus: Some(GpuRequest::Count(2)),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let devs = hc.device_requests.unwrap();
        // run_args GPU should win; only one device request
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].count, Some(2));
    }

    #[test]
    fn gpu_request_used_when_no_run_args_gpu() {
        let mut opts = minimal_opts();
        opts.gpu_request = Some(GpuRequest::All);
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let devs = hc.device_requests.unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].count, Some(-1));
    }

    #[test]
    fn run_args_network_and_resource_overrides() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            network_mode: Some("host".to_string()),
            memory: Some(1_073_741_824),
            shm_size: Some(67_108_864),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.network_mode, Some("host".to_string()));
        assert_eq!(hc.memory, Some(1_073_741_824));
        assert_eq!(hc.shm_size, Some(67_108_864));
    }

    #[test]
    fn run_args_restart_policy_always() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            restart_policy: Some("always".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let rp = hc.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::ALWAYS));
        assert!(rp.maximum_retry_count.is_none());
    }

    #[test]
    fn run_args_restart_policy_on_failure_with_count() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            restart_policy: Some("on-failure:5".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let rp = hc.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::ON_FAILURE));
        assert_eq!(rp.maximum_retry_count, Some(5));
    }

    #[test]
    fn run_args_restart_policy_unless_stopped() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            restart_policy: Some("unless-stopped".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let rp = hc.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::UNLESS_STOPPED));
    }

    #[test]
    fn run_args_restart_policy_unknown_maps_to_empty() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            restart_policy: Some("garbage".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let rp = hc.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::EMPTY));
    }

    #[test]
    fn run_args_log_driver_with_opts() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            log_driver: Some("json-file".to_string()),
            log_opt: std::iter::once(("max-size".to_string(), "10m".to_string())).collect(),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let lc = hc.log_config.unwrap();
        assert_eq!(lc.typ, Some("json-file".to_string()));
        assert!(lc.config.is_some());
    }

    #[test]
    fn run_args_log_opt_without_driver() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            log_opt: std::iter::once(("max-size".to_string(), "5m".to_string())).collect(),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let lc = hc.log_config.unwrap();
        assert!(lc.typ.is_none());
        assert!(lc.config.is_some());
    }

    #[test]
    fn run_args_cgroupns_host() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            cgroupns_mode: Some("host".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(
            hc.cgroupns_mode,
            Some(bollard::models::HostConfigCgroupnsModeEnum::HOST)
        );
    }

    #[test]
    fn run_args_cgroupns_private() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            cgroupns_mode: Some("private".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(
            hc.cgroupns_mode,
            Some(bollard::models::HostConfigCgroupnsModeEnum::PRIVATE)
        );
    }

    #[test]
    fn run_args_devices() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            devices: vec![cella_backend::DeviceSpec {
                path_on_host: "/dev/sda".to_string(),
                path_in_container: "/dev/xvda".to_string(),
                cgroup_permissions: "rwm".to_string(),
            }],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let devs = hc.devices.unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].path_on_host, Some("/dev/sda".to_string()));
        assert_eq!(devs[0].path_in_container, Some("/dev/xvda".to_string()));
        assert_eq!(devs[0].cgroup_permissions, Some("rwm".to_string()));
    }

    #[test]
    fn run_args_ulimits() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            ulimits: vec![cella_backend::UlimitSpec {
                name: "nofile".to_string(),
                soft: 1024,
                hard: 2048,
            }],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let ulimits = hc.ulimits.unwrap();
        assert_eq!(ulimits.len(), 1);
        assert_eq!(ulimits[0].name, Some("nofile".to_string()));
        assert_eq!(ulimits[0].soft, Some(1024));
        assert_eq!(ulimits[0].hard, Some(2048));
    }

    #[test]
    fn run_args_init() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            init: Some(true),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.init, Some(true));
    }

    #[test]
    fn no_mounts_produces_none() {
        let opts = minimal_opts();
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert!(hc.mounts.is_none());
    }

    // -----------------------------------------------------------------------
    // Resource override tests
    // -----------------------------------------------------------------------

    #[test]
    fn run_args_memory_swap_override() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            memory_swap: Some(2_147_483_648),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.memory_swap, Some(2_147_483_648));
    }

    #[test]
    fn run_args_memory_reservation_override() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            memory_reservation: Some(536_870_912),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.memory_reservation, Some(536_870_912));
    }

    #[test]
    fn run_args_nano_cpus_override() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            nano_cpus: Some(2_000_000_000),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.nano_cpus, Some(2_000_000_000));
    }

    #[test]
    fn run_args_cpu_shares_override() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            cpu_shares: Some(512),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.cpu_shares, Some(512));
    }

    #[test]
    fn run_args_cpu_period_and_quota() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            cpu_period: Some(100_000),
            cpu_quota: Some(50_000),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.cpu_period, Some(100_000));
        assert_eq!(hc.cpu_quota, Some(50_000));
    }

    #[test]
    fn run_args_cpuset_cpus_and_mems() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            cpuset_cpus: Some("0,1".to_string()),
            cpuset_mems: Some("0".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.cpuset_cpus, Some("0,1".to_string()));
        assert_eq!(hc.cpuset_mems, Some("0".to_string()));
    }

    #[test]
    fn run_args_pids_limit() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            pids_limit: Some(100),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.pids_limit, Some(100));
    }

    // -----------------------------------------------------------------------
    // Security override tests
    // -----------------------------------------------------------------------

    #[test]
    fn run_args_userns_mode() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            userns_mode: Some("host".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.userns_mode, Some("host".to_string()));
    }

    #[test]
    fn run_args_cgroup_parent() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            cgroup_parent: Some("/my-cgroup".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.cgroup_parent, Some("/my-cgroup".to_string()));
    }

    // -----------------------------------------------------------------------
    // Misc override tests
    // -----------------------------------------------------------------------

    #[test]
    fn run_args_dns_overrides() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            dns: vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(
            hc.dns,
            Some(vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()])
        );
    }

    #[test]
    fn run_args_dns_search() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            dns_search: vec!["example.com".to_string()],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.dns_search, Some(vec!["example.com".to_string()]));
    }

    #[test]
    fn run_args_sysctls() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            sysctls: std::iter::once(("net.core.somaxconn".to_string(), "1024".to_string()))
                .collect(),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let sysctls = hc.sysctls.unwrap();
        assert_eq!(sysctls.get("net.core.somaxconn"), Some(&"1024".to_string()));
    }

    #[test]
    fn run_args_tmpfs() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            tmpfs: std::iter::once(("/run".to_string(), "size=64m".to_string())).collect(),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let tmpfs = hc.tmpfs.unwrap();
        assert_eq!(tmpfs.get("/run"), Some(&"size=64m".to_string()));
    }

    #[test]
    fn run_args_pid_mode() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            pid_mode: Some("host".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.pid_mode, Some("host".to_string()));
    }

    #[test]
    fn run_args_ipc_mode() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            ipc_mode: Some("host".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.ipc_mode, Some("host".to_string()));
    }

    #[test]
    fn run_args_uts_mode() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            uts_mode: Some("host".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.uts_mode, Some("host".to_string()));
    }

    #[test]
    fn run_args_runtime() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            runtime: Some("nvidia".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.runtime, Some("nvidia".to_string()));
    }

    #[test]
    fn run_args_storage_opt() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            storage_opt: std::iter::once(("size".to_string(), "10G".to_string())).collect(),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let so = hc.storage_opt.unwrap();
        assert_eq!(so.get("size"), Some(&"10G".to_string()));
    }

    #[test]
    fn run_args_device_cgroup_rules() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            device_cgroup_rules: vec!["c 1:3 mr".to_string()],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.device_cgroup_rules, Some(vec!["c 1:3 mr".to_string()]));
    }

    #[test]
    fn run_args_log_driver_without_opts() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            log_driver: Some("syslog".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let lc = hc.log_config.unwrap();
        assert_eq!(lc.typ, Some("syslog".to_string()));
        assert!(lc.config.is_none());
    }

    #[test]
    fn run_args_restart_policy_on_failure_without_count() {
        let mut opts = minimal_opts();
        opts.run_args_overrides = Some(RunArgsOverrides {
            restart_policy: Some("on-failure".to_string()),
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let rp = hc.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::ON_FAILURE));
        assert!(rp.maximum_retry_count.is_none());
    }

    // -----------------------------------------------------------------------
    // Security opt merging tests
    // -----------------------------------------------------------------------

    #[test]
    fn run_args_security_opt_merged_with_base() {
        let mut opts = minimal_opts();
        opts.security_opt = vec!["apparmor=unconfined".to_string()];
        opts.run_args_overrides = Some(RunArgsOverrides {
            security_opt: vec!["no-new-privileges".to_string()],
            ..Default::default()
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let so = hc.security_opt.unwrap();
        assert!(so.contains(&"apparmor=unconfined".to_string()));
        assert!(so.contains(&"no-new-privileges".to_string()));
    }

    // -----------------------------------------------------------------------
    // Port binding tests
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_port_bindings_for_same_port() {
        let mut opts = minimal_opts();
        opts.port_bindings.insert(
            "8080".to_string(),
            vec![
                cella_backend::PortForward {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some("8080".to_string()),
                },
                cella_backend::PortForward {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some("8081".to_string()),
                },
            ],
        );
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let pb = hc.port_bindings.unwrap();
        let bindings = pb.get("8080/tcp").unwrap().as_ref().unwrap();
        assert_eq!(bindings.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Mount consistency tests
    // -----------------------------------------------------------------------

    #[test]
    fn mount_without_consistency() {
        let m = MountConfig {
            mount_type: "bind".to_string(),
            source: "/host".to_string(),
            target: "/container".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        };
        let bollard_mount = to_bollard_mount(&m).unwrap();
        assert!(bollard_mount.consistency.is_none());
    }

    #[test]
    fn mount_with_delegated_consistency() {
        let m = MountConfig {
            mount_type: "bind".to_string(),
            source: "/host".to_string(),
            target: "/container".to_string(),
            consistency: Some("delegated".to_string()),
            read_only: false,
            external: false,
        };
        let bollard_mount = to_bollard_mount(&m).unwrap();
        assert_eq!(bollard_mount.consistency, Some("delegated".to_string()));
    }

    // -----------------------------------------------------------------------
    // Workspace mount tests
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_mount_only() {
        let mut opts = minimal_opts();
        opts.workspace_mount = Some(MountConfig {
            mount_type: "bind".to_string(),
            source: "/home/user/project".to_string(),
            target: "/workspace".to_string(),
            consistency: Some("cached".to_string()),
            read_only: false,
            external: false,
        });
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        let mounts = hc.mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, Some("/home/user/project".to_string()));
    }

    // -----------------------------------------------------------------------
    // GPU request edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn gpu_request_count_zero() {
        let req = GpuRequest::Count(0);
        let dev = gpu_request_to_device_request(&req);
        assert_eq!(dev.count, Some(0));
    }

    #[test]
    fn gpu_request_empty_device_ids() {
        let req = GpuRequest::DeviceIds(Vec::new());
        let dev = gpu_request_to_device_request(&req);
        assert_eq!(dev.device_ids, Some(Vec::new()));
        assert!(dev.count.is_none());
    }

    #[test]
    fn no_gpu_request_no_device_requests() {
        let opts = minimal_opts();
        let config = to_bollard_config(&opts);
        let hc = config.host_config.unwrap();
        assert!(hc.device_requests.is_none());
    }
}
