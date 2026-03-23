//! Docker daemon connection management.

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;

use bollard::Docker;
use tracing::debug;

use crate::CellaDockerError;
use crate::config_map::CreateContainerOptions;
use crate::container::ContainerInfo;
use crate::exec::{ExecOptions, ExecResult, InteractiveExecOptions};
use crate::image::{BuildOptions, ImageDetails};
use crate::upload::FileToUpload;

/// Boxed future type alias for async trait methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Wrapper around the bollard Docker client.
pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    /// Connect using auto-detect (`DOCKER_HOST` env var / platform socket).
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::RuntimeNotFound` if connection fails.
    pub fn connect() -> Result<Self, CellaDockerError> {
        let docker = Docker::connect_with_local_defaults().map_err(|e| {
            CellaDockerError::RuntimeNotFound {
                message: format!("failed to connect to Docker: {e}"),
            }
        })?;
        Ok(Self { inner: docker })
    }

    /// Connect with an explicit docker host URL.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::RuntimeNotFound` if connection fails.
    pub fn connect_with_host(host: &str) -> Result<Self, CellaDockerError> {
        let docker = if host.starts_with("unix://") || host.starts_with('/') {
            let path = host.strip_prefix("unix://").unwrap_or(host);
            Docker::connect_with_socket(path, 120, bollard::API_DEFAULT_VERSION)
        } else {
            Docker::connect_with_http(host, 120, bollard::API_DEFAULT_VERSION)
        }
        .map_err(|e| CellaDockerError::RuntimeNotFound {
            message: format!("failed to connect to Docker at {host}: {e}"),
        })?;
        Ok(Self { inner: docker })
    }

    /// Ping the daemon to verify connection.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` if ping fails.
    pub async fn ping(&self) -> Result<(), CellaDockerError> {
        self.inner.ping().await?;
        debug!("Docker daemon is reachable");
        Ok(())
    }

    /// Access the inner bollard client.
    pub const fn inner(&self) -> &Docker {
        &self.inner
    }
}

// ---------------------------------------------------------------------------
// DockerApi trait
// ---------------------------------------------------------------------------

/// Trait abstracting Docker operations for testability.
///
/// Uses `BoxFuture` return types for object safety and delegation support.
pub trait DockerApi: Send + Sync {
    // -- Container operations --

    fn find_container<'a>(
        &'a self,
        workspace_root: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ContainerInfo>, CellaDockerError>>;

    fn create_container<'a>(
        &'a self,
        opts: &'a CreateContainerOptions,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>>;

    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), CellaDockerError>>;

    fn stop_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), CellaDockerError>>;

    fn remove_container<'a>(
        &'a self,
        id: &'a str,
        remove_volumes: bool,
    ) -> BoxFuture<'a, Result<(), CellaDockerError>>;

    fn inspect_container<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInfo, CellaDockerError>>;

    fn list_cella_containers(
        &self,
        running_only: bool,
    ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, CellaDockerError>>;

    fn container_logs<'a>(
        &'a self,
        id: &'a str,
        tail: u32,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>>;

    // -- Exec operations --

    fn exec_command<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResult, CellaDockerError>>;

    fn exec_stream<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
        stdout_writer: Box<dyn Write + Send + 'a>,
        stderr_writer: Box<dyn Write + Send + 'a>,
    ) -> BoxFuture<'a, Result<ExecResult, CellaDockerError>>;

    fn exec_interactive<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a InteractiveExecOptions,
    ) -> BoxFuture<'a, Result<i64, CellaDockerError>>;

    fn exec_detached<'a>(
        &'a self,
        container_id: &'a str,
        opts: &'a ExecOptions,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>>;

    // -- Image operations --

    fn pull_image<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), CellaDockerError>>;

    fn build_image<'a>(
        &'a self,
        opts: &'a BuildOptions,
    ) -> BoxFuture<'a, Result<String, CellaDockerError>>;

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, CellaDockerError>>;

    fn inspect_image_details<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<ImageDetails, CellaDockerError>>;

    // -- Upload operations --

    fn upload_files<'a>(
        &'a self,
        container_id: &'a str,
        files: &'a [FileToUpload],
    ) -> BoxFuture<'a, Result<(), CellaDockerError>>;
}

// ---------------------------------------------------------------------------
// MockDockerClient (test-only)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use crate::CellaDockerError;
    use crate::config_map::CreateContainerOptions;
    use crate::container::ContainerInfo;
    use crate::exec::{ExecOptions, ExecResult, InteractiveExecOptions};
    use crate::image::{BuildOptions, ImageDetails};
    use crate::upload::FileToUpload;

    use super::{BoxFuture, DockerApi};

    /// Recorded call to the mock.
    #[derive(Debug, Clone)]
    pub enum MockCall {
        FindContainer {
            workspace_root: PathBuf,
        },
        CreateContainer {
            name: String,
        },
        StartContainer {
            id: String,
        },
        StopContainer {
            id: String,
        },
        RemoveContainer {
            id: String,
            remove_volumes: bool,
        },
        InspectContainer {
            id: String,
        },
        ListCellaContainers {
            running_only: bool,
        },
        ContainerLogs {
            id: String,
            tail: u32,
        },
        ExecCommand {
            container_id: String,
            cmd: Vec<String>,
        },
        ExecStream {
            container_id: String,
            cmd: Vec<String>,
        },
        ExecInteractive {
            container_id: String,
            cmd: Vec<String>,
        },
        ExecDetached {
            container_id: String,
            cmd: Vec<String>,
        },
        PullImage {
            image: String,
        },
        BuildImage {
            image_name: String,
        },
        ImageExists {
            image: String,
        },
        InspectImageDetails {
            image: String,
        },
        UploadFiles {
            container_id: String,
            count: usize,
        },
    }

    /// Hand-rolled mock with FIFO response queues per method and call recording.
    #[derive(Default)]
    pub struct MockDockerClient {
        pub calls: Mutex<Vec<MockCall>>,
        pub find_container_responses:
            Mutex<VecDeque<Result<Option<ContainerInfo>, CellaDockerError>>>,
        pub create_container_responses: Mutex<VecDeque<Result<String, CellaDockerError>>>,
        pub start_container_responses: Mutex<VecDeque<Result<(), CellaDockerError>>>,
        pub stop_container_responses: Mutex<VecDeque<Result<(), CellaDockerError>>>,
        pub remove_container_responses: Mutex<VecDeque<Result<(), CellaDockerError>>>,
        pub inspect_container_responses: Mutex<VecDeque<Result<ContainerInfo, CellaDockerError>>>,
        pub list_cella_containers_responses:
            Mutex<VecDeque<Result<Vec<ContainerInfo>, CellaDockerError>>>,
        pub container_logs_responses: Mutex<VecDeque<Result<String, CellaDockerError>>>,
        pub exec_command_responses: Mutex<VecDeque<Result<ExecResult, CellaDockerError>>>,
        pub exec_stream_responses: Mutex<VecDeque<Result<ExecResult, CellaDockerError>>>,
        pub exec_interactive_responses: Mutex<VecDeque<Result<i64, CellaDockerError>>>,
        pub exec_detached_responses: Mutex<VecDeque<Result<String, CellaDockerError>>>,
        pub pull_image_responses: Mutex<VecDeque<Result<(), CellaDockerError>>>,
        pub build_image_responses: Mutex<VecDeque<Result<String, CellaDockerError>>>,
        pub image_exists_responses: Mutex<VecDeque<Result<bool, CellaDockerError>>>,
        pub inspect_image_details_responses:
            Mutex<VecDeque<Result<ImageDetails, CellaDockerError>>>,
        pub upload_files_responses: Mutex<VecDeque<Result<(), CellaDockerError>>>,
    }

    impl MockDockerClient {
        pub fn new() -> Self {
            Self::default()
        }

        fn record(&self, call: MockCall) {
            self.calls.lock().unwrap().push(call);
        }

        /// # Panics
        ///
        /// Panics if the internal mutex is poisoned.
        pub fn get_calls(&self) -> Vec<MockCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl DockerApi for MockDockerClient {
        fn find_container(
            &self,
            workspace_root: &Path,
        ) -> BoxFuture<'_, Result<Option<ContainerInfo>, CellaDockerError>> {
            self.record(MockCall::FindContainer {
                workspace_root: workspace_root.to_path_buf(),
            });
            let result = self
                .find_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no find_container response configured");
            Box::pin(async move { result })
        }

        fn create_container(
            &self,
            opts: &CreateContainerOptions,
        ) -> BoxFuture<'_, Result<String, CellaDockerError>> {
            self.record(MockCall::CreateContainer {
                name: opts.name.clone(),
            });
            let result = self
                .create_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no create_container response configured");
            Box::pin(async move { result })
        }

        fn start_container(&self, id: &str) -> BoxFuture<'_, Result<(), CellaDockerError>> {
            self.record(MockCall::StartContainer { id: id.to_string() });
            let result = self
                .start_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no start_container response configured");
            Box::pin(async move { result })
        }

        fn stop_container(&self, id: &str) -> BoxFuture<'_, Result<(), CellaDockerError>> {
            self.record(MockCall::StopContainer { id: id.to_string() });
            let result = self
                .stop_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no stop_container response configured");
            Box::pin(async move { result })
        }

        fn remove_container(
            &self,
            id: &str,
            remove_volumes: bool,
        ) -> BoxFuture<'_, Result<(), CellaDockerError>> {
            self.record(MockCall::RemoveContainer {
                id: id.to_string(),
                remove_volumes,
            });
            let result = self
                .remove_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no remove_container response configured");
            Box::pin(async move { result })
        }

        fn inspect_container(
            &self,
            id: &str,
        ) -> BoxFuture<'_, Result<ContainerInfo, CellaDockerError>> {
            self.record(MockCall::InspectContainer { id: id.to_string() });
            let result = self
                .inspect_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no inspect_container response configured");
            Box::pin(async move { result })
        }

        fn list_cella_containers(
            &self,
            running_only: bool,
        ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, CellaDockerError>> {
            self.record(MockCall::ListCellaContainers { running_only });
            let result = self
                .list_cella_containers_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no list_cella_containers response configured");
            Box::pin(async move { result })
        }

        fn container_logs(
            &self,
            id: &str,
            tail: u32,
        ) -> BoxFuture<'_, Result<String, CellaDockerError>> {
            self.record(MockCall::ContainerLogs {
                id: id.to_string(),
                tail,
            });
            let result = self
                .container_logs_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no container_logs response configured");
            Box::pin(async move { result })
        }

        fn exec_command(
            &self,
            container_id: &str,
            opts: &ExecOptions,
        ) -> BoxFuture<'_, Result<ExecResult, CellaDockerError>> {
            self.record(MockCall::ExecCommand {
                container_id: container_id.to_string(),
                cmd: opts.cmd.clone(),
            });
            let result = self
                .exec_command_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no exec_command response configured");
            Box::pin(async move { result })
        }

        fn exec_stream<'a>(
            &'a self,
            container_id: &'a str,
            opts: &'a ExecOptions,
            _stdout_writer: Box<dyn Write + Send + 'a>,
            _stderr_writer: Box<dyn Write + Send + 'a>,
        ) -> BoxFuture<'a, Result<ExecResult, CellaDockerError>> {
            self.record(MockCall::ExecStream {
                container_id: container_id.to_string(),
                cmd: opts.cmd.clone(),
            });
            let result = self
                .exec_stream_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no exec_stream response configured");
            Box::pin(async move { result })
        }

        fn exec_interactive(
            &self,
            container_id: &str,
            opts: &InteractiveExecOptions,
        ) -> BoxFuture<'_, Result<i64, CellaDockerError>> {
            self.record(MockCall::ExecInteractive {
                container_id: container_id.to_string(),
                cmd: opts.cmd.clone(),
            });
            let result = self
                .exec_interactive_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no exec_interactive response configured");
            Box::pin(async move { result })
        }

        fn exec_detached(
            &self,
            container_id: &str,
            opts: &ExecOptions,
        ) -> BoxFuture<'_, Result<String, CellaDockerError>> {
            self.record(MockCall::ExecDetached {
                container_id: container_id.to_string(),
                cmd: opts.cmd.clone(),
            });
            let result = self
                .exec_detached_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no exec_detached response configured");
            Box::pin(async move { result })
        }

        fn pull_image(&self, image: &str) -> BoxFuture<'_, Result<(), CellaDockerError>> {
            self.record(MockCall::PullImage {
                image: image.to_string(),
            });
            let result = self
                .pull_image_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no pull_image response configured");
            Box::pin(async move { result })
        }

        fn build_image(
            &self,
            opts: &BuildOptions,
        ) -> BoxFuture<'_, Result<String, CellaDockerError>> {
            self.record(MockCall::BuildImage {
                image_name: opts.image_name.clone(),
            });
            let result = self
                .build_image_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no build_image response configured");
            Box::pin(async move { result })
        }

        fn image_exists(&self, image: &str) -> BoxFuture<'_, Result<bool, CellaDockerError>> {
            self.record(MockCall::ImageExists {
                image: image.to_string(),
            });
            let result = self
                .image_exists_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no image_exists response configured");
            Box::pin(async move { result })
        }

        fn inspect_image_details(
            &self,
            image: &str,
        ) -> BoxFuture<'_, Result<ImageDetails, CellaDockerError>> {
            self.record(MockCall::InspectImageDetails {
                image: image.to_string(),
            });
            let result = self
                .inspect_image_details_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no inspect_image_details response configured");
            Box::pin(async move { result })
        }

        fn upload_files(
            &self,
            container_id: &str,
            files: &[FileToUpload],
        ) -> BoxFuture<'_, Result<(), CellaDockerError>> {
            self.record(MockCall::UploadFiles {
                container_id: container_id.to_string(),
                count: files.len(),
            });
            let result = self
                .upload_files_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no upload_files response configured");
            Box::pin(async move { result })
        }
    }
}
