//! Docker daemon connection management.

use bollard::Docker;
use tracing::debug;

use crate::CellaDockerError;

/// Wrapper around the bollard Docker client.
pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    /// Connect using auto-detect with fallback discovery.
    ///
    /// Strategy:
    /// 1. Try bollard defaults (`DOCKER_HOST` env var / `/var/run/docker.sock`)
    /// 2. On failure, discover alternative sockets (docker context, known paths)
    /// 3. If all fail, return a detailed error listing everything tried
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::RuntimeNotFound` if no reachable socket is found.
    pub fn connect() -> Result<Self, CellaDockerError> {
        // Fast path: bollard defaults (DOCKER_HOST or /var/run/docker.sock)
        match Docker::connect_with_local_defaults() {
            Ok(docker) => {
                debug!("Connected to Docker via default socket");
                return Ok(Self { inner: docker });
            }
            Err(e) => {
                debug!("Default Docker connection failed: {e}, trying alternative sockets");
            }
        }

        // Fallback: discover alternative sockets
        if let Some(discovered) = crate::discovery::discover_socket() {
            let path_str = discovered.path.to_string_lossy().to_string();
            let docker = Docker::connect_with_socket(&path_str, 120, bollard::API_DEFAULT_VERSION)
                .map_err(|e| CellaDockerError::RuntimeNotFound {
                    message: format!(
                        "found socket at {path_str} (via {}) but failed to connect: {e}",
                        discovered.method,
                    ),
                })?;
            tracing::info!("Connected to Docker via discovered socket: {path_str}");
            return Ok(Self { inner: docker });
        }

        // All methods failed
        Err(CellaDockerError::RuntimeNotFound {
            message: crate::discovery::discovery_failure_message(),
        })
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
// MockDockerClient (test-only)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use cella_backend::{
        BackendError, BackendKind, BoxFuture, BuildOptions, ComposeBackend, ContainerBackend,
        ContainerInfo, CreateContainerOptions, ExecOptions, ExecResult, FileToUpload, ImageDetails,
        InteractiveExecOptions,
    };

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
        FindComposeContainer {
            project_name: String,
            service_name: String,
        },
        ListComposeContainers {
            project_name: String,
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
        pub find_container_responses: Mutex<VecDeque<Result<Option<ContainerInfo>, BackendError>>>,
        pub create_container_responses: Mutex<VecDeque<Result<String, BackendError>>>,
        pub start_container_responses: Mutex<VecDeque<Result<(), BackendError>>>,
        pub stop_container_responses: Mutex<VecDeque<Result<(), BackendError>>>,
        pub remove_container_responses: Mutex<VecDeque<Result<(), BackendError>>>,
        pub inspect_container_responses: Mutex<VecDeque<Result<ContainerInfo, BackendError>>>,
        pub list_cella_containers_responses:
            Mutex<VecDeque<Result<Vec<ContainerInfo>, BackendError>>>,
        pub find_compose_container_responses:
            Mutex<VecDeque<Result<Option<ContainerInfo>, BackendError>>>,
        pub list_compose_containers_responses:
            Mutex<VecDeque<Result<Vec<ContainerInfo>, BackendError>>>,
        pub container_logs_responses: Mutex<VecDeque<Result<String, BackendError>>>,
        pub exec_command_responses: Mutex<VecDeque<Result<ExecResult, BackendError>>>,
        pub exec_stream_responses: Mutex<VecDeque<Result<ExecResult, BackendError>>>,
        pub exec_interactive_responses: Mutex<VecDeque<Result<i64, BackendError>>>,
        pub exec_detached_responses: Mutex<VecDeque<Result<String, BackendError>>>,
        pub pull_image_responses: Mutex<VecDeque<Result<(), BackendError>>>,
        pub build_image_responses: Mutex<VecDeque<Result<String, BackendError>>>,
        pub image_exists_responses: Mutex<VecDeque<Result<bool, BackendError>>>,
        pub inspect_image_details_responses: Mutex<VecDeque<Result<ImageDetails, BackendError>>>,
        pub upload_files_responses: Mutex<VecDeque<Result<(), BackendError>>>,
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

    impl ContainerBackend for MockDockerClient {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }

        fn find_container<'a>(
            &'a self,
            workspace_root: &'a Path,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
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

        fn create_container<'a>(
            &'a self,
            opts: &'a CreateContainerOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
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

        fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            self.record(MockCall::StartContainer { id: id.to_string() });
            let result = self
                .start_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no start_container response configured");
            Box::pin(async move { result })
        }

        fn stop_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            self.record(MockCall::StopContainer { id: id.to_string() });
            let result = self
                .stop_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no stop_container response configured");
            Box::pin(async move { result })
        }

        fn remove_container<'a>(
            &'a self,
            id: &'a str,
            remove_volumes: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
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

        fn inspect_container<'a>(
            &'a self,
            id: &'a str,
        ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
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
        ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
            self.record(MockCall::ListCellaContainers { running_only });
            let result = self
                .list_cella_containers_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no list_cella_containers response configured");
            Box::pin(async move { result })
        }

        fn find_compose_service<'a>(
            &'a self,
            _project: &'a str,
            _service: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            Box::pin(async { Ok(None) })
        }

        fn container_logs<'a>(
            &'a self,
            id: &'a str,
            tail: u32,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
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

        fn exec_command<'a>(
            &'a self,
            container_id: &'a str,
            opts: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
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
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
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

        fn exec_interactive<'a>(
            &'a self,
            container_id: &'a str,
            opts: &'a InteractiveExecOptions,
        ) -> BoxFuture<'a, Result<i64, BackendError>> {
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

        fn exec_detached<'a>(
            &'a self,
            container_id: &'a str,
            opts: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
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

        fn pull_image<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
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

        fn build_image<'a>(
            &'a self,
            opts: &'a BuildOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
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

        fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
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

        fn inspect_image_details<'a>(
            &'a self,
            image: &'a str,
        ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
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

        fn upload_files<'a>(
            &'a self,
            container_id: &'a str,
            files: &'a [FileToUpload],
        ) -> BoxFuture<'a, Result<(), BackendError>> {
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

        fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn host_gateway(&self) -> &'static str {
            "host.docker.internal"
        }

        fn detect_platform(&self) -> BoxFuture<'_, Result<cella_backend::Platform, BackendError>> {
            Box::pin(async {
                Ok(cella_backend::Platform {
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                })
            })
        }

        fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
            Box::pin(async { Ok("x86_64".to_string()) })
        }

        fn inspect_image_env<'a>(
            &'a self,
            _image: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
            Box::pin(async { Ok(vec![]) })
        }

        fn inspect_image_user<'a>(
            &'a self,
            _image: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            Box::pin(async { Ok("root".to_string()) })
        }

        fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn ensure_container_network<'a>(
            &'a self,
            _container_id: &'a str,
            _repo_path: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn get_container_ip<'a>(
            &'a self,
            _container_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
            Box::pin(async { Ok(None) })
        }

        fn ensure_agent_provisioned<'a>(
            &'a self,
            _version: &'a str,
            _arch: &'a str,
            _skip_checksum: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn write_agent_addr<'a>(
            &'a self,
            _container_id: &'a str,
            _addr: &'a str,
            _token: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn agent_volume_mount(&self) -> (String, String, bool) {
            ("cella-agent".to_string(), "/cella".to_string(), true)
        }

        fn prune_old_agent_versions<'a>(
            &'a self,
            _current_version: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn update_remote_user_uid<'a>(
            &'a self,
            _container_id: &'a str,
            _remote_user: &'a str,
            _workspace_root: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl ComposeBackend for MockDockerClient {
        fn find_compose_container<'a>(
            &'a self,
            project_name: &'a str,
            service_name: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            self.record(MockCall::FindComposeContainer {
                project_name: project_name.to_string(),
                service_name: service_name.to_string(),
            });
            let result = self
                .find_compose_container_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no find_compose_container response configured");
            Box::pin(async move { result })
        }

        fn list_compose_containers<'a>(
            &'a self,
            project_name: &'a str,
        ) -> BoxFuture<'a, Result<Vec<ContainerInfo>, BackendError>> {
            self.record(MockCall::ListComposeContainers {
                project_name: project_name.to_string(),
            });
            let result = self
                .list_compose_containers_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockDockerClient: no list_compose_containers response configured");
            Box::pin(async move { result })
        }
    }
}
