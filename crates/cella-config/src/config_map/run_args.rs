//! Parse `runArgs` from devcontainer.json into Docker API overrides.
//!
//! Maps `docker create` CLI flags to bollard `HostConfig` and container body
//! fields. Unrecognized flags are collected for warning emission.

use tracing::warn;

use cella_backend::{DeviceSpec, GpuRequest, RunArgsOverrides, UlimitSpec};

/// Parse `runArgs` from devcontainer.json into overrides.
///
/// Handles `--flag value` and `--flag=value` patterns. Boolean flags like
/// `--privileged` and `--init` consume no value token.
pub fn parse_run_args(args: &[String]) -> RunArgsOverrides {
    let mut result = RunArgsOverrides::default();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        // Split --flag=value
        let (flag, inline_val) = arg.find('=').map_or((arg.as_str(), None), |eq_pos| {
            (&arg[..eq_pos], Some(&arg[eq_pos + 1..]))
        });

        // Helper: consume the next token or use inline value
        let next_val = |i: &mut usize, inline: Option<&str>| -> Option<String> {
            inline.map_or_else(
                || {
                    *i += 1;
                    args.get(*i).cloned()
                },
                |v| Some(v.to_string()),
            )
        };

        match flag {
            "--network" | "--net" | "--hostname" | "-h" | "--dns" | "--dns-search"
            | "--add-host" | "--mac-address" => {
                parse_networking_arg(flag, &mut i, inline_val, &next_val, &mut result);
            }

            "--memory"
            | "-m"
            | "--memory-swap"
            | "--memory-reservation"
            | "--cpus"
            | "--cpu-shares"
            | "-c"
            | "--cpu-period"
            | "--cpu-quota"
            | "--cpuset-cpus"
            | "--cpuset-mems"
            | "--shm-size"
            | "--pids-limit" => {
                parse_resource_arg(flag, &mut i, inline_val, &next_val, &mut result);
            }

            "--security-opt" | "--userns" | "--cgroup-parent" | "--cgroupns" => {
                parse_security_arg(flag, &mut i, inline_val, &next_val, &mut result);
            }

            "--device" | "--device-cgroup-rule" | "--gpus" => {
                parse_device_arg(flag, &mut i, inline_val, &next_val, &mut result);
            }

            "--ulimit" | "--sysctl" | "--tmpfs" | "--label" | "-l" | "--pid" | "--ipc"
            | "--uts" | "--runtime" | "--storage-opt" | "--log-driver" | "--log-opt"
            | "--restart" => {
                parse_other_arg(flag, &mut i, inline_val, &next_val, &mut result);
            }

            // -- Boolean flags (no value) --
            "--init" => result.init = Some(true),
            "--privileged" => result.privileged = Some(true),

            _ => {
                result.unrecognized.push(arg.clone());
            }
        }

        i += 1;
    }

    result
}

/// Type alias for the next-value extraction closure.
type NextValFn<'a> = dyn Fn(&mut usize, Option<&str>) -> Option<String> + 'a;

/// Parse networking-related flags.
fn parse_networking_arg(
    flag: &str,
    i: &mut usize,
    inline_val: Option<&str>,
    next_val: &NextValFn<'_>,
    result: &mut RunArgsOverrides,
) {
    match flag {
        "--network" | "--net" => {
            if let Some(v) = next_val(i, inline_val) {
                result.network_mode = Some(v);
            }
        }
        "--hostname" | "-h" => {
            if let Some(v) = next_val(i, inline_val) {
                result.hostname = Some(v);
            }
        }
        "--dns" => {
            if let Some(v) = next_val(i, inline_val) {
                result.dns.push(v);
            }
        }
        "--dns-search" => {
            if let Some(v) = next_val(i, inline_val) {
                result.dns_search.push(v);
            }
        }
        "--add-host" => {
            if let Some(v) = next_val(i, inline_val) {
                result.extra_hosts.push(v);
            }
        }
        "--mac-address" => {
            if let Some(v) = next_val(i, inline_val) {
                result.mac_address = Some(v);
            }
        }
        _ => {}
    }
}

/// Parse resource-related flags.
fn parse_resource_arg(
    flag: &str,
    i: &mut usize,
    inline_val: Option<&str>,
    next_val: &NextValFn<'_>,
    result: &mut RunArgsOverrides,
) {
    match flag {
        "--memory" | "-m" => {
            if let Some(v) = next_val(i, inline_val) {
                if let Some(bytes) = parse_byte_size(&v) {
                    result.memory = Some(bytes);
                } else {
                    warn!("runArgs: invalid --memory value: {v}");
                }
            }
        }
        "--memory-swap" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some(bytes) = parse_byte_size(&v)
            {
                result.memory_swap = Some(bytes);
            }
        }
        "--memory-reservation" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some(bytes) = parse_byte_size(&v)
            {
                result.memory_reservation = Some(bytes);
            }
        }
        "--cpus" => {
            if let Some(v) = next_val(i, inline_val)
                && let Ok(f) = v.parse::<f64>()
            {
                // CPU count * 1e9 nanoseconds; always fits in i64 for valid inputs
                #[expect(clippy::cast_possible_truncation)]
                let nano = (f * 1_000_000_000.0).round() as i64;
                result.nano_cpus = Some(nano);
            }
        }
        "--cpu-shares" | "-c" => {
            if let Some(v) = next_val(i, inline_val)
                && let Ok(n) = v.parse()
            {
                result.cpu_shares = Some(n);
            }
        }
        "--cpu-period" => {
            if let Some(v) = next_val(i, inline_val)
                && let Ok(n) = v.parse()
            {
                result.cpu_period = Some(n);
            }
        }
        "--cpu-quota" => {
            if let Some(v) = next_val(i, inline_val)
                && let Ok(n) = v.parse()
            {
                result.cpu_quota = Some(n);
            }
        }
        "--cpuset-cpus" => {
            if let Some(v) = next_val(i, inline_val) {
                result.cpuset_cpus = Some(v);
            }
        }
        "--cpuset-mems" => {
            if let Some(v) = next_val(i, inline_val) {
                result.cpuset_mems = Some(v);
            }
        }
        "--shm-size" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some(bytes) = parse_byte_size(&v)
            {
                result.shm_size = Some(bytes);
            }
        }
        "--pids-limit" => {
            if let Some(v) = next_val(i, inline_val)
                && let Ok(n) = v.parse()
            {
                result.pids_limit = Some(n);
            }
        }
        _ => {}
    }
}

/// Parse security-related flags.
fn parse_security_arg(
    flag: &str,
    i: &mut usize,
    inline_val: Option<&str>,
    next_val: &NextValFn<'_>,
    result: &mut RunArgsOverrides,
) {
    match flag {
        "--security-opt" => {
            if let Some(v) = next_val(i, inline_val) {
                result.security_opt.push(v);
            }
        }
        "--userns" => {
            if let Some(v) = next_val(i, inline_val) {
                result.userns_mode = Some(v);
            }
        }
        "--cgroup-parent" => {
            if let Some(v) = next_val(i, inline_val) {
                result.cgroup_parent = Some(v);
            }
        }
        "--cgroupns" => {
            if let Some(v) = next_val(i, inline_val) {
                result.cgroupns_mode = Some(v);
            }
        }
        _ => {}
    }
}

/// Parse device-related flags.
fn parse_device_arg(
    flag: &str,
    i: &mut usize,
    inline_val: Option<&str>,
    next_val: &NextValFn<'_>,
    result: &mut RunArgsOverrides,
) {
    match flag {
        "--device" => {
            if let Some(v) = next_val(i, inline_val) {
                result.devices.push(parse_device_spec(&v));
            }
        }
        "--device-cgroup-rule" => {
            if let Some(v) = next_val(i, inline_val) {
                result.device_cgroup_rules.push(v);
            }
        }
        "--gpus" => {
            if let Some(v) = next_val(i, inline_val) {
                result.gpus = Some(parse_gpu_spec(&v));
            }
        }
        _ => {}
    }
}

/// Parse miscellaneous flags (ulimit, sysctl, tmpfs, labels, modes, logging, etc.).
fn parse_other_arg(
    flag: &str,
    i: &mut usize,
    inline_val: Option<&str>,
    next_val: &NextValFn<'_>,
    result: &mut RunArgsOverrides,
) {
    match flag {
        "--ulimit" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some(u) = parse_ulimit(&v)
            {
                result.ulimits.push(u);
            }
        }
        "--sysctl" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some((k, val)) = v.split_once('=')
            {
                result.sysctls.insert(k.to_string(), val.to_string());
            }
        }
        "--tmpfs" => {
            if let Some(v) = next_val(i, inline_val) {
                let (path, opts) = v.split_once(':').unwrap_or((&v, ""));
                result.tmpfs.insert(path.to_string(), opts.to_string());
            }
        }
        "--label" | "-l" => {
            if let Some(v) = next_val(i, inline_val) {
                if let Some((k, val)) = v.split_once('=') {
                    result.labels.insert(k.to_string(), val.to_string());
                } else {
                    result.labels.insert(v, String::new());
                }
            }
        }
        "--pid" => {
            if let Some(v) = next_val(i, inline_val) {
                result.pid_mode = Some(v);
            }
        }
        "--ipc" => {
            if let Some(v) = next_val(i, inline_val) {
                result.ipc_mode = Some(v);
            }
        }
        "--uts" => {
            if let Some(v) = next_val(i, inline_val) {
                result.uts_mode = Some(v);
            }
        }
        "--runtime" => {
            if let Some(v) = next_val(i, inline_val) {
                result.runtime = Some(v);
            }
        }
        "--storage-opt" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some((k, val)) = v.split_once('=')
            {
                result.storage_opt.insert(k.to_string(), val.to_string());
            }
        }
        "--log-driver" => {
            if let Some(v) = next_val(i, inline_val) {
                result.log_driver = Some(v);
            }
        }
        "--log-opt" => {
            if let Some(v) = next_val(i, inline_val)
                && let Some((k, val)) = v.split_once('=')
            {
                result.log_opt.insert(k.to_string(), val.to_string());
            }
        }
        "--restart" => {
            if let Some(v) = next_val(i, inline_val) {
                result.restart_policy = Some(v);
            }
        }
        _ => {}
    }
}

/// Parse a byte size string (e.g., "512m", "2g", "1024") into bytes.
pub fn parse_byte_size(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }

    let multipliers: &[(&str, i64)] = &[
        ("tb", 1024 * 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("mb", 1024 * 1024),
        ("kb", 1024),
        ("t", 1024 * 1024 * 1024 * 1024),
        ("g", 1024 * 1024 * 1024),
        ("m", 1024 * 1024),
        ("k", 1024),
        ("b", 1),
    ];

    for (suffix, mult) in multipliers {
        if let Some(num_str) = s.strip_suffix(suffix) {
            return num_str.trim().parse::<i64>().ok().map(|n| n * mult);
        }
    }

    s.parse::<i64>().ok()
}

/// Parse `--gpus` value into a `GpuRequest`.
fn parse_gpu_spec(s: &str) -> GpuRequest {
    let s = s.trim();
    if s == "all" {
        return GpuRequest::All;
    }
    if let Ok(n) = s.parse::<i64>() {
        return GpuRequest::Count(n);
    }
    // Handle device=0,1 or count=N
    for part in s.split(',') {
        let part = part.trim();
        if let Some(ids) = part.strip_prefix("device=") {
            return GpuRequest::DeviceIds(ids.split(',').map(|s| s.trim().to_string()).collect());
        }
        if let Some(count) = part.strip_prefix("count=")
            && let Ok(n) = count.trim().parse::<i64>()
        {
            return GpuRequest::Count(n);
        }
    }
    // Fallback: treat as "all" for unrecognized GPU specs
    GpuRequest::All
}

/// Parse `--device` value: `/dev/foo:/dev/bar:rwm` or `/dev/foo`.
fn parse_device_spec(s: &str) -> DeviceSpec {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    DeviceSpec {
        path_on_host: parts[0].to_string(),
        path_in_container: parts.get(1).unwrap_or(&parts[0]).to_string(),
        cgroup_permissions: parts.get(2).unwrap_or(&"rwm").to_string(),
    }
}

/// Parse `--ulimit` value: `name=soft:hard` or `name=value`.
fn parse_ulimit(s: &str) -> Option<UlimitSpec> {
    let (name, limits) = s.split_once('=')?;
    let (soft, hard) = if let Some((s, h)) = limits.split_once(':') {
        (s.parse().ok()?, h.parse().ok()?)
    } else {
        let val = limits.parse().ok()?;
        (val, val)
    };
    Some(UlimitSpec {
        name: name.to_string(),
        soft,
        hard,
    })
}

/// Emit warnings for any unrecognized flags.
pub fn warn_unrecognized(overrides: &RunArgsOverrides) {
    for flag in &overrides.unrecognized {
        warn!("runArgs: unrecognized flag \"{flag}\" will be ignored");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).to_string()).collect()
    }

    // -- Format tests --

    #[test]
    fn parse_flag_equals_value() {
        let r = parse_run_args(&args(&["--memory=512m"]));
        assert_eq!(r.memory, Some(512 * 1024 * 1024));
    }

    #[test]
    fn parse_flag_space_value() {
        let r = parse_run_args(&args(&["--memory", "512m"]));
        assert_eq!(r.memory, Some(512 * 1024 * 1024));
    }

    #[test]
    fn parse_boolean_flag() {
        let r = parse_run_args(&args(&["--privileged"]));
        assert_eq!(r.privileged, Some(true));
    }

    // -- Networking --

    #[test]
    fn parse_network() {
        let r = parse_run_args(&args(&["--network", "host"]));
        assert_eq!(r.network_mode.as_deref(), Some("host"));
    }

    #[test]
    fn parse_net_alias() {
        let r = parse_run_args(&args(&["--net=bridge"]));
        assert_eq!(r.network_mode.as_deref(), Some("bridge"));
    }

    #[test]
    fn parse_hostname() {
        let r = parse_run_args(&args(&["--hostname", "myhost"]));
        assert_eq!(r.hostname.as_deref(), Some("myhost"));
    }

    #[test]
    fn parse_dns_multiple() {
        let r = parse_run_args(&args(&["--dns", "8.8.8.8", "--dns", "8.8.4.4"]));
        assert_eq!(r.dns, vec!["8.8.8.8", "8.8.4.4"]);
    }

    #[test]
    fn parse_add_host() {
        let r = parse_run_args(&args(&["--add-host", "myhost:192.168.1.1"]));
        assert_eq!(r.extra_hosts, vec!["myhost:192.168.1.1"]);
    }

    // -- Resources --

    #[test]
    fn parse_cpus_float() {
        let r = parse_run_args(&args(&["--cpus", "1.5"]));
        assert_eq!(r.nano_cpus, Some(1_500_000_000));
    }

    #[test]
    fn parse_shm_size() {
        let r = parse_run_args(&args(&["--shm-size=2g"]));
        assert_eq!(r.shm_size, Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_pids_limit() {
        let r = parse_run_args(&args(&["--pids-limit", "100"]));
        assert_eq!(r.pids_limit, Some(100));
    }

    #[test]
    fn parse_cpu_shares() {
        let r = parse_run_args(&args(&["--cpu-shares", "512"]));
        assert_eq!(r.cpu_shares, Some(512));
    }

    // -- Devices / GPU --

    #[test]
    fn parse_device() {
        let r = parse_run_args(&args(&["--device", "/dev/snd:/dev/snd:rw"]));
        assert_eq!(r.devices.len(), 1);
        assert_eq!(r.devices[0].path_on_host, "/dev/snd");
        assert_eq!(r.devices[0].path_in_container, "/dev/snd");
        assert_eq!(r.devices[0].cgroup_permissions, "rw");
    }

    #[test]
    fn parse_device_simple() {
        let r = parse_run_args(&args(&["--device", "/dev/fuse"]));
        assert_eq!(r.devices[0].path_on_host, "/dev/fuse");
        assert_eq!(r.devices[0].path_in_container, "/dev/fuse");
        assert_eq!(r.devices[0].cgroup_permissions, "rwm");
    }

    #[test]
    fn parse_gpus_all() {
        let r = parse_run_args(&args(&["--gpus", "all"]));
        assert_eq!(r.gpus, Some(GpuRequest::All));
    }

    #[test]
    fn parse_gpus_count() {
        let r = parse_run_args(&args(&["--gpus", "2"]));
        assert_eq!(r.gpus, Some(GpuRequest::Count(2)));
    }

    #[test]
    fn parse_gpus_device_ids() {
        let r = parse_run_args(&args(&["--gpus", "device=0,1"]));
        // "device=0,1" gets split on first comma which is inside the device= value
        // The entire "device=0,1" is one arg so parse_gpu_spec handles it
        assert!(matches!(r.gpus, Some(GpuRequest::DeviceIds(_))));
    }

    // -- Other --

    #[test]
    fn parse_ulimit() {
        let r = parse_run_args(&args(&["--ulimit", "nofile=1024:2048"]));
        assert_eq!(r.ulimits.len(), 1);
        assert_eq!(r.ulimits[0].name, "nofile");
        assert_eq!(r.ulimits[0].soft, 1024);
        assert_eq!(r.ulimits[0].hard, 2048);
    }

    #[test]
    fn parse_ulimit_single_value() {
        let r = parse_run_args(&args(&["--ulimit", "nofile=1024"]));
        assert_eq!(r.ulimits[0].soft, 1024);
        assert_eq!(r.ulimits[0].hard, 1024);
    }

    #[test]
    fn parse_sysctl() {
        let r = parse_run_args(&args(&["--sysctl", "net.core.somaxconn=1024"]));
        assert_eq!(r.sysctls.get("net.core.somaxconn").unwrap(), "1024");
    }

    #[test]
    fn parse_tmpfs() {
        let r = parse_run_args(&args(&["--tmpfs", "/tmp:rw,size=65536k"]));
        assert_eq!(r.tmpfs.get("/tmp").unwrap(), "rw,size=65536k");
    }

    #[test]
    fn parse_label() {
        let r = parse_run_args(&args(&["--label", "env=production"]));
        assert_eq!(r.labels.get("env").unwrap(), "production");
    }

    #[test]
    fn parse_label_no_value() {
        let r = parse_run_args(&args(&["-l", "debug"]));
        assert_eq!(r.labels.get("debug").unwrap(), "");
    }

    #[test]
    fn parse_pid_ipc() {
        let r = parse_run_args(&args(&["--pid", "host", "--ipc", "host"]));
        assert_eq!(r.pid_mode.as_deref(), Some("host"));
        assert_eq!(r.ipc_mode.as_deref(), Some("host"));
    }

    #[test]
    fn parse_init() {
        let r = parse_run_args(&args(&["--init"]));
        assert_eq!(r.init, Some(true));
    }

    #[test]
    fn parse_restart() {
        let r = parse_run_args(&args(&["--restart", "unless-stopped"]));
        assert_eq!(r.restart_policy.as_deref(), Some("unless-stopped"));
    }

    #[test]
    fn parse_log_driver_and_opt() {
        let r = parse_run_args(&args(&[
            "--log-driver",
            "json-file",
            "--log-opt",
            "max-size=10m",
        ]));
        assert_eq!(r.log_driver.as_deref(), Some("json-file"));
        assert_eq!(r.log_opt.get("max-size").unwrap(), "10m");
    }

    // -- Unrecognized --

    #[test]
    fn unrecognized_flags_collected() {
        let r = parse_run_args(&args(&["--unknown-flag", "--memory", "512m"]));
        assert_eq!(r.unrecognized, vec!["--unknown-flag"]);
        assert_eq!(r.memory, Some(512 * 1024 * 1024));
    }

    // -- Byte size helper --

    #[test]
    fn byte_size_gb() {
        assert_eq!(parse_byte_size("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_byte_size("2gb"), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn byte_size_mb() {
        assert_eq!(parse_byte_size("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_byte_size("512mb"), Some(512 * 1024 * 1024));
    }

    #[test]
    fn byte_size_kb() {
        assert_eq!(parse_byte_size("64k"), Some(64 * 1024));
        assert_eq!(parse_byte_size("64kb"), Some(64 * 1024));
    }

    #[test]
    fn byte_size_plain_bytes() {
        assert_eq!(parse_byte_size("1048576"), Some(1_048_576));
    }

    #[test]
    fn byte_size_case_insensitive() {
        assert_eq!(parse_byte_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_byte_size("512M"), Some(512 * 1024 * 1024));
    }

    #[test]
    fn byte_size_empty() {
        assert_eq!(parse_byte_size(""), None);
    }

    // -- Combined --

    #[test]
    fn parse_multiple_flags() {
        let r = parse_run_args(&args(&[
            "--network",
            "host",
            "--memory",
            "2g",
            "--privileged",
            "--gpus",
            "all",
            "--shm-size=64m",
        ]));
        assert_eq!(r.network_mode.as_deref(), Some("host"));
        assert_eq!(r.memory, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(r.privileged, Some(true));
        assert_eq!(r.gpus, Some(GpuRequest::All));
        assert_eq!(r.shm_size, Some(64 * 1024 * 1024));
    }

    #[test]
    fn parse_empty_args() {
        let r = parse_run_args(&[]);
        assert!(r.network_mode.is_none());
        assert!(r.unrecognized.is_empty());
    }
}
