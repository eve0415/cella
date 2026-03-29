//! `ContainerBackend` implementation for the Apple Container runtime.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use cella_backend::{
    BackendError, BackendKind, BoxFuture, BuildOptions, ContainerBackend, ContainerInfo,
    ContainerState, CreateContainerOptions, ExecOptions, ExecResult, FileToUpload, ImageDetails,
    InteractiveExecOptions, MountInfo, PortBinding,
};
use tracing::{debug, warn};

use crate::sdk::ContainerCli;
use crate::sdk::types::ContainerListEntry;

/// Apple Container backend — drives the `container` CLI binary.
pub struct AppleContainerBackend {
    cli: ContainerCli,
    staging_base: PathBuf,
}

impl AppleContainerBackend {
    /// Create a new backend wrapping the given CLI handle.
    pub fn new(cli: ContainerCli) -> Self {
        warn!(
            "Apple Container backend is EXPERIMENTAL — \
             expect rough edges and missing features"
        );
        let staging_base = default_staging_base();
        Self { cli, staging_base }
    }
}

/// Default host-side staging directory for file uploads.
fn default_staging_base() -> PathBuf {
    std::env::var("HOME").map_or_else(
        |_| PathBuf::from("/tmp/cella/containers"),
        |h| {
            PathBuf::from(h)
                .join(".cache")
                .join("cella")
                .join("containers")
        },
    )
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

impl ContainerBackend for AppleContainerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::AppleContainer
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

            let entries = self.cli.list(None).await?;

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
            let args = build_create_args(opts, &self.staging_base);
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
                let staging_dir = self.staging_base.join(id);
                if staging_dir.exists() {
                    debug!(path = %staging_dir.display(), "removing staging directory");
                    let _ = tokio::fs::remove_dir_all(&staging_dir).await;
                }
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
            let entries = self.cli.list(None).await?;
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
            let build_args: Vec<(String, String)> = opts
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.cli
                .build(
                    &opts.context_path,
                    &opts.dockerfile,
                    &opts.image_name,
                    &build_args,
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
            let staging_dir = self.staging_base.join(container_id);
            tokio::fs::create_dir_all(&staging_dir).await?;

            let staging_mount = "/tmp/.cella-staging";

            for file in files {
                // Write to host staging directory.
                let host_path =
                    staging_dir.join(file.path.trim_start_matches('/').replace('/', "_"));
                tokio::fs::write(&host_path, &file.content).await?;

                let staging_name = host_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let staging_src = format!("{staging_mount}/{staging_name}");

                // Ensure parent directory exists inside the container.
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

                // Copy from staging mount to final path.
                let cp_cmd = vec!["cp".to_string(), staging_src, file.path.clone()];
                let (exit_code, _, stderr) = self
                    .cli
                    .exec_capture(container_id, &cp_cmd, Some("root"), None, None)
                    .await?;
                if exit_code != 0 {
                    return Err(BackendError::Runtime(
                        format!("cp failed for {}: {stderr}", file.path).into(),
                    ));
                }

                // Set permissions.
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build CLI arguments for `container create` from `CreateContainerOptions`.
fn build_create_args(opts: &CreateContainerOptions, staging_base: &Path) -> Vec<String> {
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
        args.push(format!("{}:{}", wm.source, wm.target));
    }

    // Additional mounts
    for mount in &opts.mounts {
        args.push("--volume".to_string());
        args.push(format!("{}:{}", mount.source, mount.target));
    }

    // Staging directory mount for file uploads.
    let staging_dir = staging_base.join(&opts.name);
    args.push("--volume".to_string());
    args.push(format!("{}:/tmp/.cella-staging", staging_dir.display()));

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

/// Emit warnings for Docker-specific options that Apple Container does not support.
fn emit_unsupported_warnings(opts: &CreateContainerOptions) {
    if opts.privileged {
        warn!("--privileged is not supported by Apple Container; ignoring");
    }
    if !opts.cap_add.is_empty() {
        warn!(
            caps = ?opts.cap_add,
            "--cap-add is not supported by Apple Container; ignoring"
        );
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

    let exit_code = entry.status.as_ref().and_then(|s| s.exit_code);

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
                        protocol: p.protocol.clone().unwrap_or_else(|| "tcp".to_string()),
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
                    mount_type: m.mount_type.clone().unwrap_or_default(),
                    source: m.source.clone().unwrap_or_default(),
                    destination: m.destination.clone().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    let created_at = labels.get("dev.cella.created_at").cloned();

    Some(ContainerInfo {
        id,
        name: config.name.clone().unwrap_or_default(),
        state,
        exit_code,
        labels,
        config_hash,
        ports,
        created_at,
        container_user: None,
        image: config.image.clone(),
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
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

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
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

        let label_idx = args.iter().position(|a| a == "--label").unwrap();
        assert_eq!(args[label_idx + 1], "dev.cella.tool=cella");
    }

    #[test]
    fn build_create_args_with_env() {
        let mut opts = minimal_create_opts();
        opts.env = vec!["FOO=bar".to_string()];
        opts.remote_env = vec!["BAZ=qux".to_string()];
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

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
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

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
        });
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

        let vol_idx = args.iter().position(|a| a == "--volume").unwrap();
        assert_eq!(args[vol_idx + 1], "/host/project:/workspace");
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
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

        let port_idx = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[port_idx + 1], "3000:8080/tcp");
    }

    #[test]
    fn build_create_args_with_entrypoint() {
        let mut opts = minimal_create_opts();
        opts.entrypoint = Some(vec!["/bin/sh".to_string(), "-c".to_string()]);
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

        let ep_idx = args.iter().position(|a| a == "--entrypoint").unwrap();
        assert_eq!(args[ep_idx + 1], "/bin/sh -c");
    }

    #[test]
    fn build_create_args_with_cmd() {
        let mut opts = minimal_create_opts();
        opts.cmd = Some(vec!["sleep".to_string(), "infinity".to_string()]);
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

        // Image should be followed by cmd.
        let img_idx = args.iter().position(|a| a == "ubuntu:latest").unwrap();
        assert_eq!(args[img_idx + 1], "sleep");
        assert_eq!(args[img_idx + 2], "infinity");
    }

    #[test]
    fn build_create_args_staging_mount() {
        let opts = minimal_create_opts();
        let staging = PathBuf::from("/tmp/staging");
        let args = build_create_args(&opts, &staging);

        // Should contain a volume mount for the staging directory.
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
        let staging_mount = vol_args.iter().find(|v| v.contains(".cella-staging"));
        assert!(staging_mount.is_some(), "expected staging volume mount");
    }

    #[test]
    fn entry_to_container_info_basic() {
        let entry = ContainerListEntry {
            status: Some(crate::sdk::types::ContainerStatus {
                state: Some("running".to_string()),
                exit_code: Some(0),
            }),
            configuration: Some(crate::sdk::types::ContainerConfiguration {
                id: Some("abc123".to_string()),
                name: Some("test".to_string()),
                image: Some("ubuntu:latest".to_string()),
                labels: Some(HashMap::from([(
                    "dev.cella.tool".to_string(),
                    "cella".to_string(),
                )])),
                published_ports: None,
                mounts: None,
            }),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.id, "abc123");
        assert_eq!(info.name, "test");
        assert_eq!(info.state, ContainerState::Running);
        assert_eq!(info.exit_code, Some(0));
        assert_eq!(info.backend, BackendKind::AppleContainer);
    }

    #[test]
    fn entry_to_container_info_stopped_state() {
        let entry = ContainerListEntry {
            status: Some(crate::sdk::types::ContainerStatus {
                state: Some("stopped".to_string()),
                exit_code: Some(137),
            }),
            configuration: Some(crate::sdk::types::ContainerConfiguration {
                id: Some("def456".to_string()),
                name: None,
                image: None,
                labels: None,
                published_ports: None,
                mounts: None,
            }),
        };

        let info = entry_to_container_info(&entry).unwrap();
        assert_eq!(info.state, ContainerState::Stopped);
        assert_eq!(info.exit_code, Some(137));
    }

    #[test]
    fn entry_to_container_info_no_id_returns_none() {
        let entry = ContainerListEntry {
            status: None,
            configuration: Some(crate::sdk::types::ContainerConfiguration {
                id: None,
                name: Some("orphan".to_string()),
                image: None,
                labels: None,
                published_ports: None,
                mounts: None,
            }),
        };

        assert!(entry_to_container_info(&entry).is_none());
    }

    #[test]
    fn entry_to_container_info_no_config_returns_none() {
        let entry = ContainerListEntry {
            status: Some(crate::sdk::types::ContainerStatus {
                state: Some("running".to_string()),
                exit_code: None,
            }),
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
    fn default_staging_base_contains_cella() {
        let base = default_staging_base();
        let base_str = base.to_string_lossy();
        assert!(
            base_str.contains("cella"),
            "staging base should contain 'cella': {base_str}"
        );
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
}
