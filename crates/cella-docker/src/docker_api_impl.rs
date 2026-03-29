//! `ContainerBackend` and `ComposeBackend` trait implementations for `DockerClient`.

use std::io::Write;
use std::path::Path;

use cella_backend::{
    BackendError, BackendKind, BoxFuture, BuildOptions, ComposeBackend, ContainerBackend,
    ContainerInfo, CreateContainerOptions, ExecOptions, ExecResult, FileToUpload, ImageDetails,
    InteractiveExecOptions,
};

use crate::client::DockerClient;

#[allow(unconditional_recursion)] // false positive: delegates to inherent methods, not trait
#[allow(clippy::use_self)] // Self::method would call the trait method (recursion); must use DockerClient::
impl ContainerBackend for DockerClient {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
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
}

#[allow(unconditional_recursion)] // false positive: delegates to inherent methods, not trait
#[allow(clippy::use_self)] // Self::method would call the trait method (recursion); must use DockerClient::
impl ComposeBackend for DockerClient {
    fn find_compose_container<'a>(
        &'a self,
        project_name: &'a str,
        service_name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            DockerClient::find_compose_container(self, project_name, service_name)
                .await
                .map_err(BackendError::from)
        })
    }

    fn list_compose_containers<'a>(
        &'a self,
        project_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ContainerInfo>, BackendError>> {
        Box::pin(async move {
            DockerClient::list_compose_containers(self, project_name)
                .await
                .map_err(BackendError::from)
        })
    }
}
