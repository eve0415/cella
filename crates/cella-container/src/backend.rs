//! `ContainerBackend` implementation for the Apple Container runtime.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use cella_backend::{
    BackendCapabilities, BackendError, BackendKind, BoxFuture, BuildOptions, ContainerBackend,
    ContainerInfo, ContainerState, CreateContainerOptions, ExecOptions, ExecResult, FileToUpload,
    ImageDetails, InteractiveExecOptions, MountInfo, Platform, PortBinding, RunArgsOverrides,
};
use tracing::{debug, warn};

use crate::sdk::ContainerCli;
use crate::sdk::types::{ContainerListEntry, FilesystemType};

/// Apple Container backend — drives the `container` CLI binary.
pub struct AppleContainerBackend {
    cli: ContainerCli,
}

impl AppleContainerBackend {
    /// Create a new backend wrapping the given CLI handle.
    pub fn new(cli: ContainerCli) -> Self {
        warn!(
            "Apple Container backend is EXPERIMENTAL — \
             expect rough edges and missing features"
        );
        Self { cli }
    }
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

impl ContainerBackend for AppleContainerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::AppleContainer
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            compose: false,
            managed_agent: false,
        }
    }

    // -- Container operations --

    fn find_container<'a>(
        &'a self,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            let canonical = workspace_root
                .canonicalize()
                .unwrap_or_else(|_| workspace_root.to_path_buf());
            let canonical_str = canonical.to_string_lossy();

            let entries = self.cli.list().await?;

            for entry in entries {
                if let Some(info) = entry_to_container_info(&entry)
                    && info
                        .labels
                        .get("dev.cella.workspace_path")
                        .is_some_and(|wp| wp == canonical_str.as_ref())
                {
                    return Ok(Some(info));
                }
            }
            Ok(None)
        })
    }

    fn create_container<'a>(
        &'a self,
        opts: &'a CreateContainerOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let args = build_create_args(opts);
            debug!(?args, "container create arguments");
            self.cli.create(&args).await
        })
    }

    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move { self.cli.start(id).await })
    }

    fn stop_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move { self.cli.stop(id).await })
    }

    fn remove_container<'a>(
        &'a self,
        id: &'a str,
        remove_volumes: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            self.cli.rm(id).await?;
            if remove_volumes {
                // Apple Container's delete never removes volumes, and this
                // backend creates no per-container anonymous volumes — named
                // volumes from user config are intentionally left in place.
                debug!("remove_volumes requested; no backend-managed volumes to remove");
            }
            Ok(())
        })
    }

    fn inspect_container<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
        Box::pin(async move {
            let entry = self.cli.inspect(id).await?;
            entry_to_container_info(&entry).ok_or_else(|| BackendError::ContainerNotFound {
                identifier: id.to_string(),
            })
        })
    }

    fn list_cella_containers(
        &self,
        running_only: bool,
    ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            let entries = self.cli.list().await?;
            let mut results = Vec::new();
            for entry in &entries {
                if let Some(info) = entry_to_container_info(entry) {
                    // Only include containers managed by cella.
                    if info.labels.contains_key("dev.cella.tool") {
                        if running_only && info.state != ContainerState::Running {
                            continue;
                        }
                        results.push(info);
                    }
                }
            }
            Ok(results)
        })
    }

    fn find_compose_service<'a>(
        &'a self,
        _project: &'a str,
        _service: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        // Apple Container does not support Docker Compose.
        Box::pin(async { Ok(None) })
    }

    fn find_container_by_label<'a>(
        &'a self,
        label: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            let (key, value) = label.split_once('=').unwrap_or((label, ""));

            let entries = self.cli.list().await?;
            for entry in entries {
                if let Some(info) = entry_to_container_info(&entry)
                    && info
                        .labels
                        .get(key)
                        .is_some_and(|v| value.is_empty() || v == value)
                {
                    return Ok(Some(info));
                }
            }
            Ok(None)
        })
    }

    fn container_logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move { self.cli.logs(id, tail).await })
    }

    // -- Exec operations --

    fn exec_command<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
        Box::pin(async move {
            let (exit_code, stdout, stderr) = self
                .cli
                .exec_capture(
                    container_id,
                    &opts.cmd,
                    opts.user.as_deref(),
                    opts.env.as_deref(),
                    opts.working_dir.as_deref(),
                )
                .await?;
            Ok(ExecResult {
                exit_code,
                stdout,
                stderr,
            })
        })
    }

    fn exec_stream<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
        mut stdout_writer: Box<dyn Write + Send + 'a>,
        mut stderr_writer: Box<dyn Write + Send + 'a>,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
        Box::pin(async move {
            // For now, capture all output then write it to the writers.
            // A proper streaming implementation would use piped stdio with
            // incremental reads, but the CLI may not support streaming JSON.
            let (exit_code, stdout, stderr) = self
                .cli
                .exec_capture(
                    container_id,
                    &opts.cmd,
                    opts.user.as_deref(),
                    opts.env.as_deref(),
                    opts.working_dir.as_deref(),
                )
                .await?;

            let _ = stdout_writer.write_all(stdout.as_bytes());
            let _ = stderr_writer.write_all(stderr.as_bytes());

            Ok(ExecResult {
                exit_code,
                stdout,
                stderr,
            })
        })
    }

    fn exec_interactive<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a InteractiveExecOptions,
    ) -> BoxFuture<'a, Result<i64, BackendError>> {
        Box::pin(async move {
            let mut args = vec!["exec".to_string()];

            if opts.tty {
                args.push("-it".to_string());
            }
            if let Some(ref u) = opts.user {
                args.push("--user".to_string());
                args.push(u.clone());
            }
            if let Some(ref vars) = opts.env {
                for var in vars {
                    args.push("-e".to_string());
                    args.push(var.clone());
                }
            }
            if let Some(ref wd) = opts.working_dir {
                args.push("-w".to_string());
                args.push(wd.clone());
            }

            args.push(container_id.to_string());
            for c in &opts.cmd {
                args.push(c.clone());
            }

            let status = std::process::Command::new(self.cli.binary_path())
                .args(&args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| BackendError::HostCommandFailed {
                    command: format!("{} {}", self.cli.binary_path().display(), args.join(" ")),
                    source: e,
                })?;

            Ok(i64::from(status.code().unwrap_or(-1)))
        })
    }

    fn exec_detached<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let mut args = vec!["exec".to_string(), "-d".to_string()];

            if let Some(ref u) = opts.user {
                args.push("--user".to_string());
                args.push(u.clone());
            }
            if let Some(ref vars) = opts.env {
                for var in vars {
                    args.push("-e".to_string());
                    args.push(var.clone());
                }
            }
            if let Some(ref wd) = opts.working_dir {
                args.push("-w".to_string());
                args.push(wd.clone());
            }

            args.push(container_id.to_string());
            for c in &opts.cmd {
                args.push(c.clone());
            }

            let output = crate::sdk::run::run_cli_owned(self.cli.binary_path(), &args).await?;
            if output.exit_code != 0 {
                return Err(BackendError::Runtime(output.stderr.into()));
            }
            // Return the output as a "process ID" placeholder — the Apple
            // Container CLI may not return a real exec ID.
            Ok(output.stdout.trim().to_string())
        })
    }

    // -- Image operations --

    fn pull_image<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move { self.cli.pull(image).await })
    }

    fn build_image<'a>(
        &'a self,
        opts: &'a BuildOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let mut extra_args = Vec::new();
            if let Some(ref target) = opts.target {
                extra_args.push("--target".to_string());
                extra_args.push(target.clone());
            }
            for cache in &opts.cache_from {
                extra_args.push("--cache-from".to_string());
                extra_args.push(cache.clone());
            }
            for opt in &opts.options {
                extra_args.push(opt.clone());
            }

            let build_args: Vec<(String, String)> = opts
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.cli
                .build_with_extra_args(
                    &opts.context_path,
                    &opts.dockerfile,
                    &opts.image_name,
                    &build_args,
                    &extra_args,
                )
                .await
        })
    }

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
        Box::pin(async move { self.cli.image_exists(image).await })
    }

    fn inspect_image_details<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
        Box::pin(async move {
            // The Apple Container CLI's image inspect format is not fully
            // documented. For now we return sensible defaults and parse what
            // we can from the raw JSON output.
            // TODO: parse OCI image config for user, env, labels once the
            // CLI output format stabilizes.
            let raw = self.cli.image_inspect(image).await?;

            // Attempt to extract user and env from a JSON blob that might
            // contain Docker-style config fields.
            let (user, env, metadata) = parse_image_details_best_effort(&raw);

            Ok(ImageDetails {
                user,
                env,
                metadata,
            })
        })
    }

    // -- File injection --

    fn upload_files<'a>(
        &'a self,
        container_id: &'a str,
        files: &'a [FileToUpload],
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            for file in files {
                // Ensure the parent directory exists inside the container.
                if let Some(parent) = Path::new(&file.path).parent() {
                    let mkdir_cmd = vec![
                        "mkdir".to_string(),
                        "-p".to_string(),
                        parent.to_string_lossy().to_string(),
                    ];
                    let _ = self
                        .cli
                        .exec_capture(container_id, &mkdir_cmd, Some("root"), None, None)
                        .await;
                }

                // Stage the content in a host temp file and copy it in.
                // The temp file is removed when `tmp` drops.
                let tmp = tempfile::NamedTempFile::new()?;
                tokio::fs::write(tmp.path(), &file.content).await?;
                self.cli
                    .cp_into(tmp.path(), container_id, &file.path)
                    .await?;

                // `container cp` preserves host-side metadata; normalize to
                // the root ownership and explicit mode the old exec-based
                // copy produced.
                let chown_cmd = vec!["chown".to_string(), "0:0".to_string(), file.path.clone()];
                let (exit_code, _, stderr) = self
                    .cli
                    .exec_capture(container_id, &chown_cmd, Some("root"), None, None)
                    .await?;
                if exit_code != 0 {
                    return Err(BackendError::Runtime(
                        format!("chown failed for {}: {stderr}", file.path).into(),
                    ));
                }

                let chmod_cmd = vec![
                    "chmod".to_string(),
                    format!("{:o}", file.mode),
                    file.path.clone(),
                ];
                let (exit_code, _, stderr) = self
                    .cli
                    .exec_capture(container_id, &chmod_cmd, Some("root"), None, None)
                    .await?;
                if exit_code != 0 {
                    return Err(BackendError::Runtime(
                        format!("chmod failed for {}: {stderr}", file.path).into(),
                    ));
                }
            }

            Ok(())
        })
    }

    // -- Connectivity --

    fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move {
            // Verify the container CLI is reachable by running `container list`.
            let _ = self.cli.list().await?;
            Ok(())
        })
    }

    fn host_gateway(&self) -> &'static str {
        // Apple Container uses the standard macOS localhost; containers
        // can reach the host via this address when networking is enabled.
        "host.local"
    }

    // -- Platform detection --

    fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>> {
        Box::pin(async move {
            Ok(Platform {
                os: "linux".to_string(),
                arch: if cfg!(target_arch = "aarch64") {
                    "arm64".to_string()
                } else {
                    "amd64".to_string()
                },
            })
        })
    }

    fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
        Box::pin(async move {
            Ok(if cfg!(target_arch = "aarch64") {
                "aarch64".to_string()
            } else {
                "x86_64".to_string()
            })
        })
    }

    // -- Extended image inspection --

    fn inspect_image_env<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
        Box::pin(async move {
            let details = self.inspect_image_details(image).await?;
            Ok(details.env)
        })
    }

    fn inspect_image_user<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let details = self.inspect_image_details(image).await?;
            Ok(details.user)
        })
    }

    // -- Network operations --

    fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move {
            Err(BackendError::NotSupported {
                backend: "apple-container".to_string(),
                operation: "ensure_network".to_string(),
            })
        })
    }

    fn ensure_container_network<'a>(
        &'a self,
        _container_id: &'a str,
        _repo_path: &'a Path,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            Err(BackendError::NotSupported {
                backend: "apple-container".to_string(),
                operation: "ensure_container_network".to_string(),
            })
        })
    }

    fn get_container_ip<'a>(
        &'a self,
        _container_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
        Box::pin(async move { Ok(None) })
    }

    // -- Agent provisioning --

    fn ensure_agent_provisioned<'a>(
        &'a self,
        _version: &'a str,
        _arch: &'a str,
        _skip_checksum: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            Err(BackendError::NotSupported {
                backend: "apple-container".to_string(),
                operation: "ensure_agent_provisioned".to_string(),
            })
        })
    }

    fn write_agent_addr<'a>(
        &'a self,
        _container_id: &'a str,
        _addr: &'a str,
        _token: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            Err(BackendError::NotSupported {
                backend: "apple-container".to_string(),
                operation: "write_agent_addr".to_string(),
            })
        })
    }

    fn agent_volume_mount(&self) -> (String, String, bool) {
        // Apple Container doesn't use Docker volumes; return a
        // placeholder that the CLI can detect and skip.
        (String::new(), "/cella".to_string(), true)
    }

    fn prune_old_agent_versions<'a>(
        &'a self,
        _current_version: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build CLI arguments for `container create` from `CreateContainerOptions`.
fn build_create_args(opts: &CreateContainerOptions) -> Vec<String> {
    let mut args = Vec::new();

    // Name
    args.push("--name".to_string());
    args.push(opts.name.clone());

    // Labels
    for (key, value) in &opts.labels {
        args.push("--label".to_string());
        args.push(format!("{key}={value}"));
    }

    // Environment variables
    for var in &opts.env {
        args.push("-e".to_string());
        args.push(var.clone());
    }
    for var in &opts.remote_env {
        args.push("-e".to_string());
        args.push(var.clone());
    }

    // User
    if let Some(ref user) = opts.user {
        args.push("-u".to_string());
        args.push(user.clone());
    }

    // Working directory
    args.push("-w".to_string());
    args.push(opts.workspace_folder.clone());

    // Workspace mount
    if let Some(ref wm) = opts.workspace_mount {
        args.push("--volume".to_string());
        let ro_suffix = if wm.read_only { ":ro" } else { "" };
        args.push(format!("{}:{}{ro_suffix}", wm.source, wm.target));
    }

    // Additional mounts. Named volumes work through `-v name:target`
    // natively; tmpfs entries use the dedicated flag.
    for mount in &opts.mounts {
        if mount.mount_type == "tmpfs" {
            args.push("--tmpfs".to_string());
            args.push(mount.target.clone());
        } else {
            args.push("--volume".to_string());
            let ro_suffix = if mount.read_only { ":ro" } else { "" };
            args.push(format!("{}:{}{ro_suffix}", mount.source, mount.target));
        }
    }

    // Port bindings
    for (container_port, forwards) in &opts.port_bindings {
        for fwd in forwards {
            let host_part = match (&fwd.host_ip, &fwd.host_port) {
                (Some(ip), Some(port)) => format!("{ip}:{port}"),
                (None, Some(port)) => port.clone(),
                (Some(ip), None) => format!("{ip}:"),
                (None, None) => String::new(),
            };
            args.push("-p".to_string());
            if host_part.is_empty() {
                args.push(container_port.clone());
            } else {
                args.push(format!("{host_part}:{container_port}"));
            }
        }
    }

    // Entrypoint
    if let Some(ref ep) = opts.entrypoint
        && !ep.is_empty()
    {
        args.push("--entrypoint".to_string());
        args.push(ep.join(" "));
    }

    // Linux capabilities (supported since Apple Container 0.12.0).
    for cap in &opts.cap_add {
        args.push("--cap-add".to_string());
        args.push(cap.clone());
    }

    // runArgs overrides with native CLI equivalents.
    if let Some(ref overrides) = opts.run_args_overrides {
        push_override_args(&mut args, overrides);
    }

    // SSH agent forwarding
    if std::env::var("SSH_AUTH_SOCK").is_ok() {
        args.push("--ssh".to_string());
    }

    // Warn about unsupported Docker-specific options.
    emit_unsupported_warnings(opts);

    // Image goes last.
    args.push(opts.image.clone());

    // Command after image.
    if let Some(ref cmd) = opts.cmd {
        for c in cmd {
            args.push(c.clone());
        }
    }

    args
}

/// Append runArgs overrides that have native Apple Container equivalents.
fn push_override_args(args: &mut Vec<String>, overrides: &RunArgsOverrides) {
    if let Some(memory) = overrides.memory
        && memory > 0
    {
        args.push("--memory".to_string());
        args.push(memory.to_string());
    }

    // Docker counts in nano-CPUs; Apple Container allocates whole vCPUs.
    if let Some(nano_cpus) = overrides.nano_cpus
        && let Ok(nano) = u64::try_from(nano_cpus)
        && nano > 0
    {
        args.push("--cpus".to_string());
        args.push(nano.div_ceil(1_000_000_000).to_string());
    }

    if let Some(shm_size) = overrides.shm_size
        && shm_size > 0
    {
        args.push("--shm-size".to_string());
        args.push(shm_size.to_string());
    }

    if overrides.init == Some(true) {
        args.push("--init".to_string());
    }

    for ip in &overrides.dns {
        args.push("--dns".to_string());
        args.push(ip.clone());
    }
    for domain in &overrides.dns_search {
        args.push("--dns-search".to_string());
        args.push(domain.clone());
    }

    for ulimit in &overrides.ulimits {
        args.push("--ulimit".to_string());
        args.push(format!("{}={}:{}", ulimit.name, ulimit.soft, ulimit.hard));
    }

    for (path, options) in &overrides.tmpfs {
        if !options.is_empty() {
            warn!(
                path,
                options, "tmpfs mount options are not supported by Apple Container; mounting plain"
            );
        }
        args.push("--tmpfs".to_string());
        args.push(path.clone());
    }

    for (key, value) in &overrides.labels {
        args.push("--label".to_string());
        args.push(format!("{key}={value}"));
    }

    if let Some(ref runtime) = overrides.runtime {
        args.push("--runtime".to_string());
        args.push(runtime.clone());
    }
}

/// Collect runArgs override flags that Apple Container has no equivalent for.
fn collect_unsupported_overrides(overrides: &RunArgsOverrides) -> Vec<&'static str> {
    let mut ignored = Vec::new();

    let flags: [(bool, &'static str); 22] = [
        (overrides.network_mode.is_some(), "--network"),
        (overrides.hostname.is_some(), "--hostname"),
        (!overrides.extra_hosts.is_empty(), "--add-host"),
        (overrides.mac_address.is_some(), "--mac-address"),
        (overrides.memory_swap.is_some(), "--memory-swap"),
        (
            overrides.memory_reservation.is_some(),
            "--memory-reservation",
        ),
        (overrides.cpu_shares.is_some(), "--cpu-shares"),
        (overrides.cpu_period.is_some(), "--cpu-period"),
        (overrides.cpu_quota.is_some(), "--cpu-quota"),
        (overrides.cpuset_cpus.is_some(), "--cpuset-cpus"),
        (overrides.cpuset_mems.is_some(), "--cpuset-mems"),
        (overrides.pids_limit.is_some(), "--pids-limit"),
        (overrides.userns_mode.is_some(), "--userns"),
        (overrides.cgroup_parent.is_some(), "--cgroup-parent"),
        (overrides.cgroupns_mode.is_some(), "--cgroupns"),
        (!overrides.sysctls.is_empty(), "--sysctl"),
        (overrides.pid_mode.is_some(), "--pid"),
        (overrides.ipc_mode.is_some(), "--ipc"),
        (overrides.uts_mode.is_some(), "--uts"),
        (!overrides.storage_opt.is_empty(), "--storage-opt"),
        (
            overrides.log_driver.is_some() || !overrides.log_opt.is_empty(),
            "--log-driver/--log-opt",
        ),
        (overrides.restart_policy.is_some(), "--restart"),
    ];

    for (present, flag) in flags {
        if present {
            ignored.push(flag);
        }
    }

    ignored
}

/// Emit warnings for Docker-specific options that Apple Container does not support.
fn emit_unsupported_warnings(opts: &CreateContainerOptions) {
    if opts.privileged {
        warn!("--privileged is not supported by Apple Container; ignoring");
    }
    if !opts.security_opt.is_empty() {
        warn!(
            opts = ?opts.security_opt,
            "--security-opt is not supported by Apple Container; ignoring"
        );
    }
    if opts.gpu_request.is_some() {
        warn!("GPU passthrough is not supported by Apple Container; ignoring");
    }

    if let Some(ref overrides) = opts.run_args_overrides {
        if !overrides.devices.is_empty() {
            warn!("--device is not supported by Apple Container; ignoring");
        }
        if overrides.gpus.is_some() {
            warn!("--gpus is not supported by Apple Container; ignoring");
        }
        let ignored = collect_unsupported_overrides(overrides);
        if !ignored.is_empty() {
            warn!(
                flags = ?ignored,
                "runArgs not supported by Apple Container; ignoring"
            );
        }
        if !overrides.unrecognized.is_empty() {
            warn!(
                args = ?overrides.unrecognized,
                "unrecognized runArgs passed to Apple Container; ignoring"
            );
        }
    }
}

/// Convert a CLI list/inspect entry into a `ContainerInfo`.
///
/// Returns `None` if the entry lacks an ID (which is required).
fn entry_to_container_info(entry: &ContainerListEntry) -> Option<ContainerInfo> {
    let config = entry.configuration.as_ref()?;
    let id = config.id.clone()?;

    let state = entry
        .status
        .as_ref()
        .and_then(|s| s.state.as_deref())
        .map_or_else(
            || ContainerState::Other("unknown".to_string()),
            |s| {
                // Apple Container uses "stopped" rather than Docker's "exited".
                match s {
                    "stopped" => ContainerState::Stopped,
                    other => ContainerState::parse(other),
                }
            },
        );

    let labels = config.labels.clone().unwrap_or_default();
    let config_hash = labels.get("dev.cella.config_hash").cloned();

    let ports = config
        .published_ports
        .as_ref()
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    Some(PortBinding {
                        container_port: p.container_port?,
                        host_port: p.host_port,
                        protocol: p.proto.clone().unwrap_or_else(|| "tcp".to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let mounts = config
        .mounts
        .as_ref()
        .map(|ms| {
            ms.iter()
                .map(|m| MountInfo {
                    mount_type: m
                        .fs_type
                        .as_ref()
                        .map(|t| t.kind().to_string())
                        .unwrap_or_default(),
                    // Volume mounts surface the volume name (Docker parity);
                    // the raw source is the host-side storage path.
                    source: m
                        .fs_type
                        .as_ref()
                        .and_then(FilesystemType::volume_name)
                        .map(String::from)
                        .or_else(|| m.source.clone())
                        .unwrap_or_default(),
                    destination: m.destination.clone().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    let created_at = labels.get("dev.cella.created_at").cloned();

    Some(ContainerInfo {
        name: id.clone(),
        id,
        state,
        // 1.0.0 does not report exit codes through ls/inspect.
        exit_code: None,
        labels,
        config_hash,
        ports,
        created_at,
        container_user: None,
        image: config.image.as_ref().and_then(|i| i.reference.clone()),
        mounts,
        backend: BackendKind::AppleContainer,
    })
}

/// Best-effort extraction of user, env, and metadata from raw image inspect JSON.
fn parse_image_details_best_effort(raw: &str) -> (String, Vec<String>, Option<String>) {
    let mut user = "root".to_string();
    let mut env = Vec::new();
    let mut metadata = None;

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        // Try Docker-style config paths.
        let config = value
            .get("Config")
            .or_else(|| value.get("config"))
            .or_else(|| value.get("container_config"));

        if let Some(cfg) = config {
            if let Some(u) = cfg.get("User").or_else(|| cfg.get("user"))
                && let Some(u_str) = u.as_str()
                && !u_str.is_empty()
            {
                // Take only the user part (before any colon).
                user = u_str.split(':').next().unwrap_or(u_str).to_string();
            }
            if let Some(e) = cfg.get("Env").or_else(|| cfg.get("env"))
                && let Some(arr) = e.as_array()
            {
                env = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(labels) = cfg.get("Labels").or_else(|| cfg.get("labels"))
                && let Some(md) = labels.get("devcontainer.metadata").and_then(|v| v.as_str())
            {
                metadata = Some(md.to_string());
            }
        }
    }

    (user, env, metadata)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cella_backend::{
        CreateContainerOptions, GpuRequest, MountConfig, PortForward, RunArgsOverrides,
    };

    use super::*;

    fn minimal_create_opts() -> CreateContainerOptions {
        CreateContainerOptions {
            name: "test-container".to_string(),
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
    fn build_create_args_minimal() {
        let opts = minimal_create_opts();
        let args = build_create_args(&opts);

        assert!(args.contains(&"--name".to_string()));
        assert!(args.contains(&"test-container".to_string()));
        assert!(args.contains(&"-w".to_string()));
        assert!(args.contains(&"/workspace".to_string()));
        // Image should be the last non-cmd argument.
        assert_eq!(args.last().unwrap(), "ubuntu:latest");
    }

    #[test]
    fn build_create_args_with_labels() {
        let mut opts = minimal_create_opts();
        opts.labels
            .insert("dev.cella.tool".to_string(), "cella".to_string());
        let args = build_create_args(&opts);

        let label_idx = args.iter().position(|a| a == "--label").unwrap();
        assert_eq!(args[label_idx + 1], "dev.cella.tool=cella");
    }

    #[test]
    fn build_create_args_with_env() {
        let mut opts = minimal_create_opts();
        opts.env = vec!["FOO=bar".to_string()];
        opts.remote_env = vec!["BAZ=qux".to_string()];
        let args = build_create_args(&opts);

        let env_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| if a == "-e" { Some(i) } else { None })
            .collect();
        assert_eq!(env_positions.len(), 2);
        assert_eq!(args[env_positions[0] + 1], "FOO=bar");
        assert_eq!(args[env_positions[1] + 1], "BAZ=qux");
    }

    #[test]
    fn build_create_args_with_user() {
        let mut opts = minimal_create_opts();
        opts.user = Some("vscode".to_string());
        let args = build_create_args(&opts);

        let user_idx = args.iter().position(|a| a == "-u").unwrap();
        assert_eq!(args[user_idx + 1], "vscode");
    }

    #[test]
    fn build_create_args_with_workspace_mount() {
        let mut opts = minimal_create_opts();
        opts.workspace_mount = Some(MountConfig {
            mount_type: "bind".to_string(),
            source: "/host/project".to_string(),
            target: "/workspace".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        });
        let args = build_create_args(&opts);

        let vol_idx = args.iter().position(|a| a == "--volume").unwrap();
        assert_eq!(args[vol_idx + 1], "/host/project:/workspace");
    }

    #[test]
    fn build_create_args_workspace_mount_read_only() {
        let mut opts = minimal_create_opts();
        opts.workspace_mount = Some(MountConfig {
            mount_type: "bind".to_string(),
            source: "/host/project".to_string(),
            target: "/workspace".to_string(),
            consistency: None,
            read_only: true,
            external: false,
        });
        let args = build_create_args(&opts);

        let vol_idx = args.iter().position(|a| a == "--volume").unwrap();
        assert_eq!(
            args[vol_idx + 1],
            "/host/project:/workspace:ro",
            "read_only workspace mount must append :ro"
        );
    }

    #[test]
    fn build_create_args_with_ports() {
        let mut opts = minimal_create_opts();
        opts.port_bindings.insert(
            "8080/tcp".to_string(),
            vec![PortForward {
                host_ip: None,
                host_port: Some("3000".to_string()),
            }],
        );
        let args = build_create_args(&opts);

        let port_idx = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[port_idx + 1], "3000:8080/tcp");
    }

    #[test]
    fn build_create_args_with_entrypoint() {
        let mut opts = minimal_create_opts();
        opts.entrypoint = Some(vec!["/bin/sh".to_string(), "-c".to_string()]);
        let args = build_create_args(&opts);

        let ep_idx = args.iter().position(|a| a == "--entrypoint").unwrap();
        assert_eq!(args[ep_idx + 1], "/bin/sh -c");
    }

    #[test]
    fn build_create_args_with_cmd() {
        let mut opts = minimal_create_opts();
        opts.cmd = Some(vec!["sleep".to_string(), "infinity".to_string()]);
        let args = build_create_args(&opts);

        // Image should be followed by cmd.
        let img_idx = args.iter().position(|a| a == "ubuntu:latest").unwrap();
        assert_eq!(args[img_idx + 1], "sleep");
        assert_eq!(args[img_idx + 2], "infinity");
    }

    #[test]
    fn build_create_args_has_no_staging_mount() {
        let opts = minimal_create_opts();
        let args = build_create_args(&opts);

        assert!(
            !args.iter().any(|a| a.contains(".cella-staging")),
            "file uploads use `container cp`; no staging mount expected"
        );
    }

    /// Test helper: a status with the given state and no network attachments.
    fn test_status(state: &str) -> crate::sdk::types::ContainerStatus {
        crate::sdk::types::ContainerStatus {
            state: Some(state.to_string()),
            networks: Vec::new(),
        }
    }

    /// Test helper: a configuration with the given ID and everything else
    /// empty.
    fn test_config(id: Option<&str>) -> crate::sdk::types::ContainerConfiguration {
        crate::sdk::types::ContainerConfiguration {
            id: id.map(String::from),
            image: None,
            labels: None,
            published_ports: None,
            mounts: None,
            networks: Vec::new(),
        }
    }

    #[test]
    fn entry_to_container_info_basic() {
        let mut config = test_config(Some("abc123"));
        config.image = Some(crate::sdk::types::ImageDescription {
            reference: Some("ubuntu:latest".to_string()),
        });
        config.labels = Some(HashMap::from([(
            "dev.cella.tool".to_string(),
            "cella".to_string(),
        )]));
        let entry = ContainerListEntry {
            status: Some(test_status("running")),
            configuration: Some(config),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.id, "abc123");
        // Containers have no separate name; the ID doubles as the name.
        assert_eq!(info.name, "abc123");
        assert_eq!(info.state, ContainerState::Running);
        assert_eq!(info.image.as_deref(), Some("ubuntu:latest"));
        assert_eq!(info.backend, BackendKind::AppleContainer);
    }

    #[test]
    fn entry_to_container_info_stopped_state() {
        let entry = ContainerListEntry {
            status: Some(test_status("stopped")),
            configuration: Some(test_config(Some("def456"))),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.state, ContainerState::Stopped);
        // 1.0.0 does not report exit codes through ls/inspect.
        assert_eq!(info.exit_code, None);
    }

    #[test]
    fn entry_to_container_info_no_id_returns_none() {
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(test_config(None)),
        };

        assert!(entry_to_container_info(&entry).is_none());
    }

    #[test]
    fn entry_to_container_info_no_config_returns_none() {
        let entry = ContainerListEntry {
            status: Some(test_status("running")),
            configuration: None,
        };

        assert!(entry_to_container_info(&entry).is_none());
    }

    #[test]
    fn parse_image_details_docker_style() {
        let raw = r#"{
            "Config": {
                "User": "vscode:vscode",
                "Env": ["PATH=/usr/bin", "HOME=/home/vscode"],
                "Labels": {
                    "devcontainer.metadata": "[{\"remoteUser\":\"vscode\"}]"
                }
            }
        }"#;

        let (user, env, metadata) = parse_image_details_best_effort(raw);
        assert_eq!(user, "vscode");
        assert_eq!(env.len(), 2);
        assert!(metadata.is_some());
    }

    #[test]
    fn parse_image_details_empty_json() {
        let (user, env, metadata) = parse_image_details_best_effort("{}");
        assert_eq!(user, "root");
        assert!(env.is_empty());
        assert!(metadata.is_none());
    }

    #[test]
    fn parse_image_details_invalid_json() {
        let (user, env, metadata) = parse_image_details_best_effort("not json");
        assert_eq!(user, "root");
        assert!(env.is_empty());
        assert!(metadata.is_none());
    }

    #[test]
    fn build_create_args_passes_capabilities() {
        let mut opts = minimal_create_opts();
        opts.cap_add = vec!["SYS_PTRACE".to_string(), "NET_RAW".to_string()];
        let args = build_create_args(&opts);

        let cap_values: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--cap-add")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(cap_values, vec!["SYS_PTRACE", "NET_RAW"]);
    }

    #[test]
    fn push_override_args_maps_resources() {
        let overrides = RunArgsOverrides {
            memory: Some(2_147_483_648),
            nano_cpus: Some(1_500_000_000),
            shm_size: Some(67_108_864),
            init: Some(true),
            ..RunArgsOverrides::default()
        };
        let mut args = Vec::new();
        push_override_args(&mut args, &overrides);

        let pairs: Vec<(&str, &str)> = args
            .windows(2)
            .map(|w| (w[0].as_str(), w[1].as_str()))
            .collect();
        assert!(pairs.contains(&("--memory", "2147483648")));
        // 1.5 CPUs rounds up to 2 whole vCPUs.
        assert!(pairs.contains(&("--cpus", "2")));
        assert!(pairs.contains(&("--shm-size", "67108864")));
        assert!(args.contains(&"--init".to_string()));
    }

    #[test]
    fn push_override_args_maps_dns_and_ulimits() {
        let overrides = RunArgsOverrides {
            dns: vec!["1.1.1.1".to_string()],
            dns_search: vec!["internal.example".to_string()],
            ulimits: vec![cella_backend::UlimitSpec {
                name: "nofile".to_string(),
                soft: 1024,
                hard: 4096,
            }],
            ..RunArgsOverrides::default()
        };
        let mut args = Vec::new();
        push_override_args(&mut args, &overrides);

        let pairs: Vec<(&str, &str)> = args
            .windows(2)
            .map(|w| (w[0].as_str(), w[1].as_str()))
            .collect();
        assert!(pairs.contains(&("--dns", "1.1.1.1")));
        assert!(pairs.contains(&("--dns-search", "internal.example")));
        assert!(pairs.contains(&("--ulimit", "nofile=1024:4096")));
    }

    #[test]
    fn push_override_args_maps_tmpfs_labels_runtime() {
        let overrides = RunArgsOverrides {
            tmpfs: HashMap::from([("/scratch".to_string(), String::new())]),
            labels: HashMap::from([("from.runargs".to_string(), "yes".to_string())]),
            runtime: Some("container-runtime-linux".to_string()),
            ..RunArgsOverrides::default()
        };
        let mut args = Vec::new();
        push_override_args(&mut args, &overrides);

        let pairs: Vec<(&str, &str)> = args
            .windows(2)
            .map(|w| (w[0].as_str(), w[1].as_str()))
            .collect();
        assert!(pairs.contains(&("--tmpfs", "/scratch")));
        assert!(pairs.contains(&("--label", "from.runargs=yes")));
        assert!(pairs.contains(&("--runtime", "container-runtime-linux")));
    }

    #[test]
    fn push_override_args_skips_non_positive_resources() {
        let overrides = RunArgsOverrides {
            memory: Some(0),
            nano_cpus: Some(-1),
            shm_size: Some(0),
            ..RunArgsOverrides::default()
        };
        let mut args = Vec::new();
        push_override_args(&mut args, &overrides);
        assert!(args.is_empty(), "non-positive resources must be skipped");
    }

    #[test]
    fn build_create_args_tmpfs_mount_config() {
        let mut opts = minimal_create_opts();
        opts.mounts = vec![MountConfig {
            mount_type: "tmpfs".to_string(),
            source: String::new(),
            target: "/run/scratch".to_string(),
            consistency: None,
            read_only: false,
            external: false,
        }];
        let args = build_create_args(&opts);

        let mut pairs = args.windows(2).map(|w| (w[0].as_str(), w[1].as_str()));
        assert!(pairs.any(|p| p == ("--tmpfs", "/run/scratch")));
        assert!(
            !args.iter().any(|a| a.starts_with(":/run/scratch")),
            "tmpfs must not be emitted as an empty-source volume"
        );
    }

    #[test]
    fn collect_unsupported_overrides_lists_flags() {
        let overrides = RunArgsOverrides {
            network_mode: Some("host".to_string()),
            hostname: Some("custom".to_string()),
            sysctls: HashMap::from([("net.core.somaxconn".to_string(), "1024".to_string())]),
            restart_policy: Some("always".to_string()),
            ..RunArgsOverrides::default()
        };
        let ignored = collect_unsupported_overrides(&overrides);
        assert!(ignored.contains(&"--network"));
        assert!(ignored.contains(&"--hostname"));
        assert!(ignored.contains(&"--sysctl"));
        assert!(ignored.contains(&"--restart"));
        assert_eq!(ignored.len(), 4);
    }

    #[test]
    fn collect_unsupported_overrides_empty_for_default() {
        assert!(collect_unsupported_overrides(&RunArgsOverrides::default()).is_empty());
    }

    #[test]
    fn unsupported_warnings_do_not_panic() {
        let mut opts = minimal_create_opts();
        opts.privileged = true;
        opts.cap_add = vec!["SYS_PTRACE".to_string()];
        opts.security_opt = vec!["seccomp=unconfined".to_string()];
        opts.gpu_request = Some(GpuRequest::All);
        opts.run_args_overrides = Some(RunArgsOverrides {
            gpus: Some(GpuRequest::All),
            unrecognized: vec!["--custom-flag".to_string()],
            ..RunArgsOverrides::default()
        });

        // Should not panic — just emits warnings.
        emit_unsupported_warnings(&opts);
    }

    // -- Additional build_create_args edge cases ------------------------------

    #[test]
    fn build_create_args_with_additional_mounts() {
        let mut opts = minimal_create_opts();
        opts.mounts = vec![
            MountConfig {
                mount_type: "bind".to_string(),
                source: "/host/data".to_string(),
                target: "/data".to_string(),
                consistency: None,
                read_only: false,
                external: false,
            },
            MountConfig {
                mount_type: "volume".to_string(),
                source: "vol1".to_string(),
                target: "/vol".to_string(),
                consistency: None,
                read_only: false,
                external: false,
            },
        ];
        let args = build_create_args(&opts);

        let vol_args: Vec<&str> = args
            .windows(2)
            .filter_map(|w| {
                if w[0] == "--volume" {
                    Some(w[1].as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            vol_args.contains(&"/host/data:/data"),
            "expected /host/data:/data mount, got: {vol_args:?}"
        );
        assert!(
            vol_args.contains(&"vol1:/vol"),
            "expected vol1:/vol mount, got: {vol_args:?}"
        );
    }

    #[test]
    fn build_create_args_additional_mount_read_only() {
        let mut opts = minimal_create_opts();
        opts.mounts = vec![MountConfig {
            mount_type: "bind".to_string(),
            source: "/host/data".to_string(),
            target: "/data".to_string(),
            consistency: None,
            read_only: true,
            external: false,
        }];
        let args = build_create_args(&opts);

        let vol_args: Vec<&str> = args
            .windows(2)
            .filter_map(|w| {
                if w[0] == "--volume" {
                    Some(w[1].as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            vol_args.contains(&"/host/data:/data:ro"),
            "read_only additional mount must append :ro; got: {vol_args:?}"
        );
    }

    #[test]
    fn build_create_args_port_with_host_ip() {
        let mut opts = minimal_create_opts();
        opts.port_bindings.insert(
            "443/tcp".to_string(),
            vec![PortForward {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some("8443".to_string()),
            }],
        );
        let args = build_create_args(&opts);

        let port_idx = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[port_idx + 1], "127.0.0.1:8443:443/tcp");
    }

    #[test]
    fn build_create_args_port_with_host_ip_only() {
        let mut opts = minimal_create_opts();
        opts.port_bindings.insert(
            "80/tcp".to_string(),
            vec![PortForward {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: None,
            }],
        );
        let args = build_create_args(&opts);

        let port_idx = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[port_idx + 1], "0.0.0.0::80/tcp");
    }

    #[test]
    fn build_create_args_port_no_host() {
        let mut opts = minimal_create_opts();
        opts.port_bindings.insert(
            "5432/tcp".to_string(),
            vec![PortForward {
                host_ip: None,
                host_port: None,
            }],
        );
        let args = build_create_args(&opts);

        let port_idx = args.iter().position(|a| a == "-p").unwrap();
        // When both host_ip and host_port are None, just the container port.
        assert_eq!(args[port_idx + 1], "5432/tcp");
    }

    #[test]
    fn build_create_args_multiple_port_forwards() {
        let mut opts = minimal_create_opts();
        opts.port_bindings.insert(
            "8080/tcp".to_string(),
            vec![
                PortForward {
                    host_ip: None,
                    host_port: Some("3000".to_string()),
                },
                PortForward {
                    host_ip: None,
                    host_port: Some("3001".to_string()),
                },
            ],
        );
        let args = build_create_args(&opts);

        let port_count = args.iter().filter(|a| *a == "-p").count();
        assert_eq!(port_count, 2);
    }

    #[test]
    fn build_create_args_empty_entrypoint_is_skipped() {
        let mut opts = minimal_create_opts();
        opts.entrypoint = Some(Vec::new());
        let args = build_create_args(&opts);

        assert!(
            !args.contains(&"--entrypoint".to_string()),
            "empty entrypoint should be skipped"
        );
    }

    #[test]
    fn build_create_args_none_entrypoint_is_skipped() {
        let opts = minimal_create_opts();
        let args = build_create_args(&opts);

        assert!(
            !args.contains(&"--entrypoint".to_string()),
            "None entrypoint should be skipped"
        );
    }

    // -- Additional entry_to_container_info edge cases ------------------------

    #[test]
    fn entry_to_container_info_with_ports() {
        let mut config = test_config(Some("p1"));
        config.published_ports = Some(vec![
            crate::sdk::types::PublishedPort {
                container_port: Some(80),
                host_port: Some(8080),
                proto: Some("tcp".to_string()),
            },
            crate::sdk::types::PublishedPort {
                container_port: Some(443),
                host_port: None,
                proto: None,
            },
            // Port entry missing container_port should be filtered out.
            crate::sdk::types::PublishedPort {
                container_port: None,
                host_port: Some(9999),
                proto: None,
            },
        ]);
        let entry = ContainerListEntry {
            status: Some(test_status("running")),
            configuration: Some(config),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.ports.len(), 2);
        assert_eq!(info.ports[0].container_port, 80);
        assert_eq!(info.ports[0].host_port, Some(8080));
        assert_eq!(info.ports[0].protocol, "tcp");
        assert_eq!(info.ports[1].container_port, 443);
        assert_eq!(info.ports[1].host_port, None);
        assert_eq!(info.ports[1].protocol, "tcp"); // default
    }

    #[test]
    fn entry_to_container_info_with_mounts() {
        let mut config = test_config(Some("m1"));
        config.mounts = Some(vec![
            crate::sdk::types::MountEntry {
                source: Some("/host".to_string()),
                destination: Some("/container".to_string()),
                fs_type: Some(FilesystemType::Virtiofs(serde_json::Value::Null)),
                options: Vec::new(),
            },
            crate::sdk::types::MountEntry {
                source: Some("/var/lib/volumes/data".to_string()),
                destination: Some("/data".to_string()),
                fs_type: Some(FilesystemType::Volume {
                    name: Some("data".to_string()),
                }),
                options: Vec::new(),
            },
        ]);
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(config),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.mounts.len(), 2);
        assert_eq!(info.mounts[0].source, "/host");
        assert_eq!(info.mounts[0].destination, "/container");
        assert_eq!(info.mounts[0].mount_type, "bind");
        // Volume mounts surface the volume name, not the storage path.
        assert_eq!(info.mounts[1].source, "data");
        assert_eq!(info.mounts[1].mount_type, "volume");
    }

    #[test]
    fn entry_to_container_info_with_config_hash_label() {
        let mut config = test_config(Some("h1"));
        config.labels = Some(HashMap::from([(
            "dev.cella.config_hash".to_string(),
            "abc123hash".to_string(),
        )]));
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(config),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.config_hash.as_deref(), Some("abc123hash"));
    }

    #[test]
    fn entry_to_container_info_with_created_at_label() {
        let mut config = test_config(Some("ca1"));
        config.labels = Some(HashMap::from([(
            "dev.cella.created_at".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
        )]));
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(config),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn entry_to_container_info_unknown_state() {
        let entry = ContainerListEntry {
            status: Some(test_status("stopping")),
            configuration: Some(test_config(Some("u1"))),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.state, ContainerState::Other("stopping".to_string()));
    }

    #[test]
    fn entry_to_container_info_no_status_defaults_to_unknown() {
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(test_config(Some("ns1"))),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.state, ContainerState::Other("unknown".to_string()));
    }

    #[test]
    fn entry_to_container_info_empty_mount_fields() {
        let mut config = test_config(Some("em1"));
        config.mounts = Some(vec![crate::sdk::types::MountEntry {
            source: None,
            destination: None,
            fs_type: None,
            options: Vec::new(),
        }]);
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(config),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.mounts.len(), 1);
        assert_eq!(info.mounts[0].source, "");
        assert_eq!(info.mounts[0].destination, "");
        assert_eq!(info.mounts[0].mount_type, "");
    }

    // -- Additional parse_image_details_best_effort edge cases ----------------

    #[test]
    fn parse_image_details_lowercase_keys() {
        let raw = r#"{
            "config": {
                "user": "app",
                "env": ["PATH=/usr/bin"],
                "labels": {
                    "devcontainer.metadata": "[{}]"
                }
            }
        }"#;
        let (user, env, metadata) = parse_image_details_best_effort(raw);
        assert_eq!(user, "app");
        assert_eq!(env, vec!["PATH=/usr/bin"]);
        assert!(metadata.is_some());
    }

    #[test]
    fn parse_image_details_container_config_key() {
        let raw = r#"{
            "container_config": {
                "User": "deploy",
                "Env": ["LANG=C"]
            }
        }"#;
        let (user, env, metadata) = parse_image_details_best_effort(raw);
        assert_eq!(user, "deploy");
        assert_eq!(env, vec!["LANG=C"]);
        assert!(metadata.is_none());
    }

    #[test]
    fn parse_image_details_user_with_colon() {
        let raw = r#"{"Config": {"User": "1000:1000"}}"#;
        let (user, env, metadata) = parse_image_details_best_effort(raw);
        assert_eq!(user, "1000");
        assert!(env.is_empty());
        assert!(metadata.is_none());
    }

    #[test]
    fn parse_image_details_empty_user_stays_root() {
        let raw = r#"{"Config": {"User": ""}}"#;
        let (user, _, _) = parse_image_details_best_effort(raw);
        assert_eq!(user, "root");
    }

    #[test]
    fn parse_image_details_no_labels() {
        let raw = r#"{"Config": {"User": "web", "Env": []}}"#;
        let (user, env, metadata) = parse_image_details_best_effort(raw);
        assert_eq!(user, "web");
        assert!(env.is_empty());
        assert!(metadata.is_none());
    }

    #[test]
    fn parse_image_details_env_with_non_string_values() {
        let raw = r#"{"Config": {"Env": ["GOOD=val", 42, null]}}"#;
        let (_, env, _) = parse_image_details_best_effort(raw);
        // Only string values should be collected.
        assert_eq!(env, vec!["GOOD=val"]);
    }
}
