//! `DockerApi` trait implementation for `DockerClient` via delegation to inherent methods.

use std::io::Write;
use std::path::Path;

use crate::CellaDockerError;
use crate::client::{BoxFuture, DockerApi, DockerClient};
use crate::config_map::CreateContainerOptions;
use crate::container::ContainerInfo;
use crate::exec::{ExecOptions, ExecResult, InteractiveExecOptions};
use crate::image::{BuildOptions, ImageDetails};
use crate::upload::FileToUpload;

#[allow(unconditional_recursion)] // false positive: delegates to inherent methods, not trait
#[allow(clippy::use_self)] // Self::method would call the trait method (recursion); must use DockerClient::
impl DockerApi for DockerClient {
    fn find_container<'a>(
        &'a self,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, CellaDockerError>> {
        Box::pin(DockerClient::find_container(self, workspace_root))
    }

    fn create_container<'a>(
        &'a self,
        opts: &'a CreateContainerOptions,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>> {
        Box::pin(DockerClient::create_container(self, opts))
    }

    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), CellaDockerError>> {
        Box::pin(DockerClient::start_container(self, id))
    }

    fn stop_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), CellaDockerError>> {
        Box::pin(DockerClient::stop_container(self, id))
    }

    fn remove_container<'a>(
        &'a self,
        id: &'a str,
        remove_volumes: bool,
    ) -> BoxFuture<'a, Result<(), CellaDockerError>> {
        Box::pin(DockerClient::remove_container(self, id, remove_volumes))
    }

    fn inspect_container<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInfo, CellaDockerError>> {
        Box::pin(DockerClient::inspect_container(self, id))
    }

    fn list_cella_containers(
        &self,
        running_only: bool,
    ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, CellaDockerError>> {
        Box::pin(DockerClient::list_cella_containers(self, running_only))
    }

    fn container_logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>> {
        Box::pin(DockerClient::container_logs(self, id, tail))
    }

    fn exec_command<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResult, CellaDockerError>> {
        Box::pin(DockerClient::exec_command(self, container_id, opts))
    }

    fn exec_stream<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
        stdout_writer: Box<dyn Write + Send + 'a>,
        stderr_writer: Box<dyn Write + Send + 'a>,
    ) -> BoxFuture<'a, Result<ExecResult, CellaDockerError>> {
        Box::pin(DockerClient::exec_stream(
            self,
            container_id,
            opts,
            stdout_writer,
            stderr_writer,
        ))
    }

    fn exec_interactive<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a InteractiveExecOptions,
    ) -> BoxFuture<'a, Result<i64, CellaDockerError>> {
        Box::pin(DockerClient::exec_interactive(self, container_id, opts))
    }

    fn exec_detached<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>> {
        Box::pin(DockerClient::exec_detached(self, container_id, opts))
    }

    fn pull_image<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), CellaDockerError>> {
        Box::pin(DockerClient::pull_image(self, image))
    }

    fn build_image<'a>(
        &'a self,
        opts: &'a BuildOptions,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>> {
        Box::pin(DockerClient::build_image(self, opts, |_| {}))
    }

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, CellaDockerError>> {
        Box::pin(DockerClient::image_exists(self, image))
    }

    fn inspect_image_details<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<ImageDetails, CellaDockerError>> {
        Box::pin(DockerClient::inspect_image_details(self, image))
    }

    fn upload_files<'a>(
        &'a self,
        container_id: &'a str,
        files: &'a [FileToUpload],
    ) -> BoxFuture<'a, Result<(), CellaDockerError>> {
        Box::pin(DockerClient::upload_files(self, container_id, files))
    }
}
