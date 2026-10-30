//! A [`ContainerBackend`] for flow tests that never reaches a container
//! runtime.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use crate::error::BackendError;
use crate::traits::{BackendCapabilities, BoxFuture, ContainerBackend, Platform};
use crate::types::{
    BackendKind, BuildOptions, ContainerInfo, CreateContainerOptions, ExecOptions, ExecResult,
    FileToUpload, ImageDetails, InteractiveExecOptions,
};

/// Backend whose every runtime operation is recorded by name and fails with
/// [`BackendError::NotSupported`], so a test can see whether, and where, a
/// flow first touched the runtime.
///
/// It reports a Docker backend with compose and a managed agent.
#[derive(Debug, Default)]
pub struct RecordingBackend {
    calls: Mutex<Vec<&'static str>>,
    running: Option<ContainerInfo>,
}

impl RecordingBackend {
    /// A backend that also returns `container` from `find_compose_service`
    /// and `inspect_container`, so a flow gets past locating its container.
    pub fn with_running_container(container: ContainerInfo) -> Self {
        Self {
            calls: Mutex::default(),
            running: Some(container),
        }
    }

    /// Runtime operations called so far, in order.
    ///
    /// # Panics
    ///
    /// Panics if a previous call panicked while recording.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    /// Record `operation` and return the running container, or fail like
    /// every other operation when there is none.
    fn running<'a, T: Send + 'a>(
        &self,
        operation: &'static str,
        wrap: impl FnOnce(ContainerInfo) -> T,
    ) -> BoxFuture<'a, Result<T, BackendError>> {
        let Some(container) = self.running.clone() else {
            return self.fail(operation);
        };
        self.calls.lock().unwrap().push(operation);
        let value = wrap(container);
        Box::pin(async move { Ok(value) })
    }

    fn fail<'a, T: Send + 'a>(
        &self,
        operation: &'static str,
    ) -> BoxFuture<'a, Result<T, BackendError>> {
        self.calls.lock().unwrap().push(operation);
        Box::pin(async move {
            Err(BackendError::NotSupported {
                backend: "recording test backend".to_string(),
                operation: operation.to_string(),
            })
        })
    }
}

impl ContainerBackend for RecordingBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            compose: true,
            managed_agent: true,
            ssh_agent: cella_env::SshAgentTransport::BindMount,
        }
    }

    fn find_container<'a>(
        &'a self,
        _: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        self.fail("find_container")
    }

    fn create_container<'a>(
        &'a self,
        _: &'a CreateContainerOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        self.fail("create_container")
    }

    fn start_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("start_container")
    }

    fn stop_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("stop_container")
    }

    fn remove_container<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("remove_container")
    }

    fn inspect_container<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
        self.running("inspect_container", |container| container)
    }

    fn inspect_container_env<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
        self.fail("inspect_container_env")
    }

    fn list_cella_containers(
        &self,
        _: bool,
    ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
        self.fail("list_cella_containers")
    }

    fn find_compose_service<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        self.running("find_compose_service", Some)
    }

    fn find_container_by_label<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        self.fail("find_container_by_label")
    }

    fn container_logs<'a>(
        &'a self,
        _: &'a str,
        _: u32,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        self.fail("container_logs")
    }

    fn exec_command<'a>(
        &'a self,
        _: &'a str,
        _: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
        self.fail("exec_command")
    }

    fn exec_stream<'a>(
        &'a self,
        _: &'a str,
        _: &'a ExecOptions,
        _: Box<dyn Write + Send + 'a>,
        _: Box<dyn Write + Send + 'a>,
    ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
        self.fail("exec_stream")
    }

    fn exec_interactive<'a>(
        &'a self,
        _: &'a str,
        _: &'a InteractiveExecOptions,
    ) -> BoxFuture<'a, Result<i64, BackendError>> {
        self.fail("exec_interactive")
    }

    fn exec_detached<'a>(
        &'a self,
        _: &'a str,
        _: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        self.fail("exec_detached")
    }

    fn pull_image<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("pull_image")
    }

    fn build_image<'a>(
        &'a self,
        _: &'a BuildOptions,
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        self.fail("build_image")
    }

    fn image_exists<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
        self.fail("image_exists")
    }

    fn tag_image<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("tag_image")
    }

    fn inspect_image_details<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
        self.fail("inspect_image_details")
    }

    fn upload_files<'a>(
        &'a self,
        _: &'a str,
        _: &'a [FileToUpload],
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("upload_files")
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        self.fail("ping")
    }

    fn host_gateway(&self) -> &'static str {
        "host.docker.internal"
    }

    fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>> {
        self.fail("detect_platform")
    }

    fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
        self.fail("detect_container_arch")
    }

    fn inspect_image_env<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
        self.fail("inspect_image_env")
    }

    fn inspect_image_user<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<String, BackendError>> {
        self.fail("inspect_image_user")
    }

    fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        self.fail("ensure_network")
    }

    fn ensure_container_network<'a>(
        &'a self,
        _: &'a str,
        _: &'a Path,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("ensure_container_network")
    }

    fn get_container_ip<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
        self.fail("get_container_ip")
    }

    fn ensure_agent_provisioned<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("ensure_agent_provisioned")
    }

    fn write_agent_addr<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("write_agent_addr")
    }

    fn agent_volume_mount(&self) -> (String, String, bool) {
        self.calls.lock().unwrap().push("agent_volume_mount");
        ("cella-agent".to_string(), "/cella".to_string(), true)
    }

    fn prune_old_agent_versions<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        self.fail("prune_old_agent_versions")
    }
}
