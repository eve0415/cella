//! Container backend trait definitions.
//!
//! [`ContainerBackend`] defines the core operations that all container runtimes
//! must support. [`ComposeBackend`] is an extension trait for Docker Compose
//! support (only implemented by the Docker backend).

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;

use crate::error::BackendError;
use crate::types::{
    BackendKind, BuildOptions, ContainerInfo, CreateContainerOptions, ExecOptions, ExecResult,
    FileToUpload, ImageDetails, InteractiveExecOptions,
};

/// Boxed future type alias for async trait methods (object-safe).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Core container lifecycle trait.
///
/// Uses [`BoxFuture`] return types for object safety so callers can work with
/// `dyn ContainerBackend` trait objects.
pub trait ContainerBackend: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> BackendKind;

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
}

/// Extension trait for Docker Compose support.
///
/// Only the Docker backend implements this. Commands that require Compose
/// check for this capability via downcasting or backend kind checks.
pub trait ComposeBackend: ContainerBackend {
    fn find_compose_container<'a>(
        &'a self,
        project_name: &'a str,
        service_name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>>;

    fn list_compose_containers<'a>(
        &'a self,
        project_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ContainerInfo>, BackendError>>;
}
