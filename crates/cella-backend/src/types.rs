//! Shared types used across all container backends.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Backend kind
// ---------------------------------------------------------------------------

/// Which container backend is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Docker,
    Podman,
    AppleContainer,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Docker => f.write_str("docker"),
            Self::Podman => f.write_str("podman"),
            Self::AppleContainer => f.write_str("apple-container"),
        }
    }
}

// ---------------------------------------------------------------------------
// Container types
// ---------------------------------------------------------------------------

/// Container state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Stopped,
    Created,
    Removing,
    Other(String),
}

impl ContainerState {
    /// Parse a container state string into a `ContainerState`.
    ///
    /// Uses a different name than `from_str` to avoid confusing `clippy` with
    /// the `std::str::FromStr` trait (which returns `Result`, not `Self`).
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "exited" | "dead" => Self::Stopped,
            "created" => Self::Created,
            "removing" => Self::Removing,
            other => Self::Other(other.to_string()),
        }
    }
}

/// A port binding exposed by the container.
#[derive(Debug, Clone)]
pub struct PortBinding {
    pub container_port: u16,
    pub host_port: Option<u16>,
    pub protocol: String,
}

/// A bind mount or volume attached to the container.
#[derive(Debug, Clone)]
pub struct MountInfo {
    pub mount_type: String,
    pub source: String,
    pub destination: String,
}

/// Information about a container.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub exit_code: Option<i64>,
    pub labels: HashMap<String, String>,
    pub config_hash: Option<String>,
    pub ports: Vec<PortBinding>,
    pub created_at: Option<String>,
    /// The USER from the container's config (only populated via inspect, not list).
    pub container_user: Option<String>,
    /// The image used to create the container.
    pub image: Option<String>,
    /// Bind mounts and volumes (only populated via inspect, not list).
    pub mounts: Vec<MountInfo>,
    /// Which backend manages this container.
    pub backend: BackendKind,
}

// ---------------------------------------------------------------------------
// Exec types
// ---------------------------------------------------------------------------

/// Options for executing a command in a container (capture mode).
pub struct ExecOptions {
    pub cmd: Vec<String>,
    pub user: Option<String>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
}

/// Result of a command execution.
pub struct ExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// Options for interactive command execution.
pub struct InteractiveExecOptions {
    pub cmd: Vec<String>,
    pub user: Option<String>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub tty: bool,
}

// ---------------------------------------------------------------------------
// Image types
// ---------------------------------------------------------------------------

/// Image inspection results.
#[derive(Debug, Clone)]
pub struct ImageDetails {
    /// Normalized USER (user portion only, defaults to `"root"`).
    pub user: String,
    /// `KEY=value` environment entries from the image config.
    pub env: Vec<String>,
    /// Raw JSON from the `devcontainer.metadata` label, if present.
    pub metadata: Option<String>,
}

/// Options for building a container image from a Dockerfile.
pub struct BuildOptions {
    pub image_name: String,
    pub context_path: PathBuf,
    pub dockerfile: String,
    pub args: HashMap<String, String>,
    pub target: Option<String>,
    pub cache_from: Vec<String>,
    pub options: Vec<String>,
}

// ---------------------------------------------------------------------------
// Upload types
// ---------------------------------------------------------------------------

/// A file to upload into a container.
pub struct FileToUpload {
    /// Absolute path inside the container.
    pub path: String,
    /// File content.
    pub content: Vec<u8>,
    /// File permissions (octal, e.g., 0o600).
    pub mode: u32,
}

// ---------------------------------------------------------------------------
// Mount / port config types (for container creation)
// ---------------------------------------------------------------------------

/// A mount configuration (abstracted from Docker's Mount type).
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub mount_type: String,
    pub source: String,
    pub target: String,
    pub consistency: Option<String>,
}

/// Backend-agnostic port forward specification.
#[derive(Debug, Clone)]
pub struct PortForward {
    pub host_ip: Option<String>,
    pub host_port: Option<String>,
}

/// GPU request specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuRequest {
    All,
    Count(i64),
    DeviceIds(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ContainerState::parse
    // -----------------------------------------------------------------------

    #[test]
    fn parse_running() {
        assert_eq!(ContainerState::parse("running"), ContainerState::Running);
    }

    #[test]
    fn parse_exited() {
        assert_eq!(ContainerState::parse("exited"), ContainerState::Stopped);
    }

    #[test]
    fn parse_dead() {
        assert_eq!(ContainerState::parse("dead"), ContainerState::Stopped);
    }

    #[test]
    fn parse_created() {
        assert_eq!(ContainerState::parse("created"), ContainerState::Created);
    }

    #[test]
    fn parse_removing() {
        assert_eq!(ContainerState::parse("removing"), ContainerState::Removing);
    }

    #[test]
    fn parse_unknown_state() {
        assert_eq!(
            ContainerState::parse("paused"),
            ContainerState::Other("paused".to_string())
        );
    }

    #[test]
    fn parse_empty_string() {
        assert_eq!(
            ContainerState::parse(""),
            ContainerState::Other(String::new())
        );
    }

    // -----------------------------------------------------------------------
    // BackendKind::Display
    // -----------------------------------------------------------------------

    #[test]
    fn display_docker() {
        assert_eq!(BackendKind::Docker.to_string(), "docker");
    }

    #[test]
    fn display_podman() {
        assert_eq!(BackendKind::Podman.to_string(), "podman");
    }

    #[test]
    fn display_apple_container() {
        assert_eq!(BackendKind::AppleContainer.to_string(), "apple-container");
    }
}

// ---------------------------------------------------------------------------
// Container creation options
// ---------------------------------------------------------------------------

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
    pub port_bindings: HashMap<String, Vec<PortForward>>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub privileged: bool,
    /// Parsed `runArgs` overrides from devcontainer.json.
    pub run_args_overrides: Option<RunArgsOverrides>,
    /// GPU request from `hostRequirements.gpu` (lower precedence than runArgs `--gpus`).
    pub gpu_request: Option<GpuRequest>,
}

// ---------------------------------------------------------------------------
// RunArgs types (parsed from devcontainer.json runArgs)
// ---------------------------------------------------------------------------

/// A device specification from `--device`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    pub path_on_host: String,
    pub path_in_container: String,
    pub cgroup_permissions: String,
}

/// A ulimit specification from `--ulimit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UlimitSpec {
    pub name: String,
    pub soft: i64,
    pub hard: i64,
}

/// Parsed overrides from `runArgs` (docker create flags).
///
/// Each field maps to the corresponding container runtime host config.
/// `None`/empty means the flag was not specified (don't override).
#[derive(Debug, Clone, Default)]
pub struct RunArgsOverrides {
    // Networking
    pub network_mode: Option<String>,
    pub hostname: Option<String>,
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub extra_hosts: Vec<String>,
    pub mac_address: Option<String>,

    // Resources
    pub memory: Option<i64>,
    pub memory_swap: Option<i64>,
    pub memory_reservation: Option<i64>,
    pub nano_cpus: Option<i64>,
    pub cpu_shares: Option<i64>,
    pub cpu_period: Option<i64>,
    pub cpu_quota: Option<i64>,
    pub cpuset_cpus: Option<String>,
    pub cpuset_mems: Option<String>,
    pub shm_size: Option<i64>,
    pub pids_limit: Option<i64>,

    // Security
    pub security_opt: Vec<String>,
    pub userns_mode: Option<String>,
    pub cgroup_parent: Option<String>,
    pub cgroupns_mode: Option<String>,

    // Devices
    pub devices: Vec<DeviceSpec>,
    pub device_cgroup_rules: Vec<String>,
    pub gpus: Option<GpuRequest>,

    // Other
    pub ulimits: Vec<UlimitSpec>,
    pub sysctls: HashMap<String, String>,
    pub tmpfs: HashMap<String, String>,
    pub labels: HashMap<String, String>,
    pub pid_mode: Option<String>,
    pub ipc_mode: Option<String>,
    pub uts_mode: Option<String>,
    pub runtime: Option<String>,
    pub storage_opt: HashMap<String, String>,
    pub log_driver: Option<String>,
    pub log_opt: HashMap<String, String>,
    pub restart_policy: Option<String>,
    pub init: Option<bool>,
    pub privileged: Option<bool>,

    /// Flags not recognized by the parser (emitted as warnings).
    pub unrecognized: Vec<String>,
}
