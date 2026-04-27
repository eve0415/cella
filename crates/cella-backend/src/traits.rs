//! Container backend trait definitions.
//!
//! [`ContainerBackend`] defines the core operations that all container runtimes
//! must support.

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;

use crate::error::BackendError;
use crate::network::{ManagedNetwork, RemovalOutcome};
use crate::types::{
    BackendKind, BuildOptions, ContainerInfo, CreateContainerOptions, ExecOptions, ExecResult,
    FileToUpload, ImageDetails, InteractiveExecOptions,
};

/// Container platform information (OS and architecture).
#[derive(Debug, Clone)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

/// Capability flags exposed by a container backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub compose: bool,
    pub managed_agent: bool,
}

/// Boxed future type alias for async trait methods (object-safe).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Core container lifecycle trait.
///
/// Uses [`BoxFuture`] return types for object safety so callers can work with
/// `dyn ContainerBackend` trait objects.
pub trait ContainerBackend: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> BackendKind;

    /// Which optional behaviors this backend supports.
    fn capabilities(&self) -> BackendCapabilities;

    // -- Container operations --

    fn find_container<'a>(
        &'a self,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>>;

    fn create_container<'a>(
        &'a self,
        opts: &'a CreateContainerOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>>;

    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>>;

    fn stop_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>>;

    fn remove_container<'a>(
        &'a self,
        id: &'a str,
        remove_volumes: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>>;

    fn inspect_container<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>>;

    fn list_cella_containers(
        &self,
        running_only: bool,
    ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>>;

    /// Find a container by compose project and service labels.
    ///
    /// Searches across **all** runtime containers (not just cella-managed ones)
    /// for the `com.docker.compose.project` and `com.docker.compose.service`
    /// labels. Returns `None` if no match is found.
    fn find_compose_service<'a>(
        &'a self,
        project: &'a str,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>>;

    /// Find a container by an arbitrary label (e.g. `"key=value"` or `"key"`).
    ///
    /// Searches **all** runtime containers, not just cella-managed ones.
    /// Returns the first match, or `None`.
    fn find_container_by_label<'a>(
        &'a self,
        label: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>>;

    fn container_logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
    ) -> BoxFuture<'a, Result<String, BackendError>>;

    // -- Exec operations --

    fn exec_command<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>>;

    fn exec_stream<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
        stdout_writer: Box<dyn Write + Send + 'a>,
        stderr_writer: Box<dyn Write + Send + 'a>,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>>;

    fn exec_interactive<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a InteractiveExecOptions,
    ) -> BoxFuture<'a, Result<i64, BackendError>>;

    fn exec_detached<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>>;

    // -- Image operations --

    fn pull_image<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), BackendError>>;

    fn build_image<'a>(
        &'a self,
        opts: &'a BuildOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>>;

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, BackendError>>;

    fn inspect_image_details<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<ImageDetails, BackendError>>;

    // -- File injection --

    fn upload_files<'a>(
        &'a self,
        container_id: &'a str,
        files: &'a [FileToUpload],
    ) -> BoxFuture<'a, Result<(), BackendError>>;

    // -- Connectivity --

    /// Verify that the backend runtime is reachable.
    fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>>;

    /// The hostname used inside containers to reach the host machine.
    ///
    /// Docker: `"host.docker.internal"`, Podman: `"host.containers.internal"`.
    fn host_gateway(&self) -> &'static str;

    // -- Platform detection --

    /// Detect the runtime's OS and architecture.
    fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>>;

    /// Detect the architecture of images pulled by the runtime (e.g. `"amd64"`).
    fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>>;

    // -- Extended image inspection --

    /// Return the environment variables from an image's config.
    fn inspect_image_env<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, BackendError>>;

    /// Return the default user from an image's config (defaults to `"root"`).
    fn inspect_image_user<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<String, BackendError>>;

    // -- Network operations --

    /// Ensure the shared cella bridge network exists.
    fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>>;

    /// Ensure a container is connected to the cella network and any
    /// repo-scoped network derived from `repo_path`.
    fn ensure_container_network<'a>(
        &'a self,
        container_id: &'a str,
        repo_path: &'a Path,
    ) -> BoxFuture<'a, Result<(), BackendError>>;

    /// Return the container's IP, preferring the cella bridge network.
    fn get_container_ip<'a>(
        &'a self,
        container_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, BackendError>>;

    /// List all networks labeled `dev.cella.managed=true`, including the
    /// shared `cella` network and any per-workspace `cella-net-*`
    /// networks. Each entry reports its current attached-endpoint count.
    ///
    /// Returns `NotSupported` on backends that don't use Docker networks
    /// (e.g. apple-container).
    fn list_managed_networks(&self) -> BoxFuture<'_, Result<Vec<ManagedNetwork>, BackendError>> {
        let kind = self.kind();
        Box::pin(async move {
            Err(BackendError::NotSupported {
                backend: kind.to_string(),
                operation: "list_managed_networks".to_string(),
            })
        })
    }

    /// Remove the named network if it has zero attached containers.
    ///
    /// Intended for orphan sweeps and workspace teardown. Callers never
    /// force-disconnect endpoints; a network in use is left alone.
    ///
    /// Returns `NotSupported` on backends that don't use Docker networks.
    fn remove_network_if_orphan<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<RemovalOutcome, BackendError>> {
        let kind = self.kind();
        Box::pin(async move {
            let _ = name;
            Err(BackendError::NotSupported {
                backend: kind.to_string(),
                operation: "remove_network_if_orphan".to_string(),
            })
        })
    }

    /// Remove the per-workspace network derived from `workspace_root`,
    /// if it has zero attached containers. Convenience wrapper over
    /// [`Self::remove_network_if_orphan`] that computes the deterministic
    /// network name from the workspace path.
    ///
    /// Returns `NotSupported` on backends that don't use Docker networks.
    fn remove_workspace_network<'a>(
        &'a self,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<RemovalOutcome, BackendError>> {
        let kind = self.kind();
        Box::pin(async move {
            let _ = workspace_root;
            Err(BackendError::NotSupported {
                backend: kind.to_string(),
                operation: "remove_workspace_network".to_string(),
            })
        })
    }

    /// Connect a container to a named Docker network.
    ///
    /// Returns `NotSupported` on backends that don't use Docker networks.
    fn connect_to_network<'a>(
        &'a self,
        container_id: &'a str,
        network_name: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        let kind = self.kind();
        Box::pin(async move {
            let _ = (container_id, network_name);
            Err(BackendError::NotSupported {
                backend: kind.to_string(),
                operation: "connect_to_network".to_string(),
            })
        })
    }

    /// Check whether a named Docker network exists.
    ///
    /// Returns `NotSupported` on backends that don't use Docker networks.
    fn network_exists<'a>(
        &'a self,
        network_name: &'a str,
    ) -> BoxFuture<'a, Result<bool, BackendError>> {
        let kind = self.kind();
        Box::pin(async move {
            let _ = network_name;
            Err(BackendError::NotSupported {
                backend: kind.to_string(),
                operation: "network_exists".to_string(),
            })
        })
    }

    // -- Agent provisioning --

    /// Ensure the cella-agent binary is available for containers.
    ///
    /// For Docker this populates a shared volume; other backends may use
    /// different mechanisms.
    fn ensure_agent_provisioned<'a>(
        &'a self,
        version: &'a str,
        arch: &'a str,
        skip_checksum: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>>;

    /// Write the daemon address and auth token into a running container
    /// so the in-container agent can connect back to the host daemon.
    fn write_agent_addr<'a>(
        &'a self,
        container_id: &'a str,
        addr: &'a str,
        token: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>>;

    /// Return the (`volume_name`, `mount_target`, `read_only`) tuple for the
    /// agent binary volume mount.
    fn agent_volume_mount(&self) -> (String, String, bool);

    /// Remove agent binary versions other than `current_version`.
    fn prune_old_agent_versions<'a>(
        &'a self,
        current_version: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>>;
}
