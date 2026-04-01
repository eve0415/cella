//! `ContainerBackend` trait implementation for `DockerClient`.

use std::io::Write;
use std::path::Path;

use cella_backend::{
    BackendCapabilities, BackendError, BackendKind, BoxFuture, BuildOptions, ContainerBackend,
    ContainerInfo, CreateContainerOptions, ExecOptions, ExecResult, FileToUpload, ImageDetails,
    InteractiveExecOptions, Platform,
};

use crate::client::DockerClient;

#[allow(unconditional_recursion)] // false positive: delegates to inherent methods, not trait
#[allow(clippy::use_self)] // Self::method would call the trait method (recursion); must use DockerClient::
impl ContainerBackend for DockerClient {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            compose: true,
            managed_agent: true,
        }
    }

    fn find_container<'a>(
        &'a self,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            DockerClient::find_container(self, workspace_root)
                .await
                .map_err(BackendError::from)
        })
    }

    fn create_container<'a>(
        &'a self,
        opts: &'a CreateContainerOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            DockerClient::create_container(self, opts)
                .await
                .map_err(BackendError::from)
        })
    }

    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            DockerClient::start_container(self, id)
                .await
                .map_err(BackendError::from)
        })
    }

    fn stop_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            DockerClient::stop_container(self, id)
                .await
                .map_err(BackendError::from)
        })
    }

    fn remove_container<'a>(
        &'a self,
        id: &'a str,
        remove_volumes: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            DockerClient::remove_container(self, id, remove_volumes)
                .await
                .map_err(BackendError::from)
        })
    }

    fn inspect_container<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
        Box::pin(async move {
            DockerClient::inspect_container(self, id)
                .await
                .map_err(BackendError::from)
        })
    }

    fn list_cella_containers(
        &self,
        running_only: bool,
    ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            DockerClient::list_cella_containers(self, running_only)
                .await
                .map_err(BackendError::from)
        })
    }

    fn find_compose_service<'a>(
        &'a self,
        project: &'a str,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            DockerClient::find_compose_container(self, project, service)
                .await
                .map_err(BackendError::from)
        })
    }

    fn find_container_by_label<'a>(
        &'a self,
        label: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            use bollard::query_parameters::ListContainersOptions;
            use std::collections::HashMap;

            let filters: HashMap<String, Vec<String>> =
                HashMap::from([("label".to_string(), vec![label.to_string()])]);
            let options = ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            };
            let containers = self
                .inner()
                .list_containers(Some(options))
                .await
                .map_err(|e| BackendError::Runtime(Box::new(e)))?;

            if let Some(summary) = containers.into_iter().next() {
                let id = summary.id.as_deref().unwrap_or_default();
                let info = self.inspect_container(id).await?;
                Ok(Some(info))
            } else {
                Ok(None)
            }
        })
    }

    fn container_logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            DockerClient::container_logs(self, id, tail)
                .await
                .map_err(BackendError::from)
        })
    }

    fn exec_command<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
        Box::pin(async move {
            DockerClient::exec_command(self, container_id, opts)
                .await
                .map_err(BackendError::from)
        })
    }

    fn exec_stream<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
        stdout_writer: Box<dyn Write + Send + 'a>,
        stderr_writer: Box<dyn Write + Send + 'a>,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
        Box::pin(async move {
            DockerClient::exec_stream(self, container_id, opts, stdout_writer, stderr_writer)
                .await
                .map_err(BackendError::from)
        })
    }

    fn exec_interactive<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a InteractiveExecOptions,
    ) -> BoxFuture<'a, Result<i64, BackendError>> {
        Box::pin(async move {
            DockerClient::exec_interactive(self, container_id, opts)
                .await
                .map_err(BackendError::from)
        })
    }

    fn exec_detached<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            DockerClient::exec_detached(self, container_id, opts)
                .await
                .map_err(BackendError::from)
        })
    }

    fn pull_image<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            DockerClient::pull_image(self, image)
                .await
                .map_err(BackendError::from)
        })
    }

    fn build_image<'a>(
        &'a self,
        opts: &'a BuildOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            DockerClient::build_image(self, opts, |_| {})
                .await
                .map_err(BackendError::from)
        })
    }

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
        Box::pin(async move {
            DockerClient::image_exists(self, image)
                .await
                .map_err(BackendError::from)
        })
    }

    fn inspect_image_details<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
        Box::pin(async move {
            DockerClient::inspect_image_details(self, image)
                .await
                .map_err(BackendError::from)
        })
    }

    fn upload_files<'a>(
        &'a self,
        container_id: &'a str,
        files: &'a [FileToUpload],
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            DockerClient::upload_files(self, container_id, files)
                .await
                .map_err(BackendError::from)
        })
    }

    // -- Connectivity --

    fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move { DockerClient::ping(self).await.map_err(BackendError::from) })
    }

    fn host_gateway(&self) -> &'static str {
        "host.docker.internal"
    }

    // -- Platform detection --

    fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>> {
        Box::pin(async move {
            let version = self
                .inner()
                .version()
                .await
                .map_err(|e| BackendError::Runtime(Box::new(e)))?;
            let os = version.os.unwrap_or_else(|| "linux".to_string());
            let arch = version.arch.unwrap_or_else(|| "amd64".to_string());
            Ok(Platform { os, arch })
        })
    }

    fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
        Box::pin(async move {
            crate::volume::detect_container_arch(self.inner())
                .await
                .map_err(BackendError::from)
        })
    }

    // -- Extended image inspection --

    fn inspect_image_env<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
        Box::pin(async move {
            DockerClient::inspect_image_env(self, image)
                .await
                .map_err(BackendError::from)
        })
    }

    fn inspect_image_user<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            DockerClient::inspect_image_user(self, image)
                .await
                .map_err(BackendError::from)
        })
    }

    // -- Network operations --

    fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move {
            crate::network::ensure_network(self.inner())
                .await
                .map_err(BackendError::from)
        })
    }

    fn ensure_container_network<'a>(
        &'a self,
        container_id: &'a str,
        repo_path: &'a Path,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            crate::network::ensure_container_connected(self.inner(), container_id)
                .await
                .map_err(BackendError::from)?;
            crate::network::ensure_repo_network(self.inner(), container_id, repo_path)
                .await
                .map_err(BackendError::from)?;
            Ok(())
        })
    }

    fn get_container_ip<'a>(
        &'a self,
        container_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
        Box::pin(async move {
            Ok(crate::network::get_container_cella_ip(self.inner(), container_id).await)
        })
    }

    // -- Agent provisioning --

    fn ensure_agent_provisioned<'a>(
        &'a self,
        version: &'a str,
        arch: &'a str,
        skip_checksum: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            crate::volume::ensure_agent_volume_populated(self.inner(), arch, skip_checksum)
                .await
                .map_err(BackendError::from)?;
            // version is used for pruning, not for population — the current
            // binary is always at the compiled-in version.
            let _ = version;
            Ok(())
        })
    }

    fn write_agent_addr<'a>(
        &'a self,
        _container_id: &'a str,
        addr: &'a str,
        token: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            // Docker writes to the shared agent volume (not per-container).
            crate::volume::write_daemon_addr_file(self.inner(), addr, token)
                .await
                .map_err(BackendError::from)
        })
    }

    fn agent_volume_mount(&self) -> (String, String, bool) {
        let (name, target, ro) = crate::volume::agent_volume_mount();
        (name.to_string(), target.to_string(), ro)
    }

    fn prune_old_agent_versions<'a>(
        &'a self,
        current_version: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            crate::volume::prune_old_agent_versions(self.inner(), current_version)
                .await
                .map_err(BackendError::from)
        })
    }

    // -- UID remapping --

    fn update_remote_user_uid<'a>(
        &'a self,
        container_id: &'a str,
        remote_user: &'a str,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            crate::uid::update_remote_user_uid(self, container_id, remote_user, workspace_root)
                .await
                .map_err(BackendError::from)
        })
    }
}
