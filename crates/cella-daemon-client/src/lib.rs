//! Client for the daemon management Unix socket.

pub mod ssh_proxy;

use std::path::{Path, PathBuf};

use cella_protocol::{
    ContainerRegistrationData, ContainerSummary, ForwardedPortDetail, ManagementRequest,
    ManagementResponse,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Client for one daemon management socket.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Build a client for `socket_path`.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Send a raw management request and return the raw response.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure.
    pub async fn request(
        &self,
        request: &ManagementRequest,
    ) -> Result<ManagementResponse, DaemonClientError> {
        send_management_request(&self.socket_path, request).await
    }

    /// Ping the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or unexpected response.
    pub async fn ping(&self) -> Result<(), DaemonClientError> {
        match self.request(&ManagementRequest::Ping).await? {
            ManagementResponse::Pong => Ok(()),
            response => Err(DaemonClientError::unexpected("pong", &response)),
        }
    }

    /// Query daemon status.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or unexpected response.
    pub async fn query_status(&self) -> Result<DaemonStatus, DaemonClientError> {
        match self.request(&ManagementRequest::QueryStatus).await? {
            ManagementResponse::Status {
                pid,
                uptime_secs,
                container_count,
                containers,
                is_orbstack,
                daemon_version,
                daemon_started_at,
                control_port,
                control_token,
            } => Ok(DaemonStatus {
                pid,
                uptime_secs,
                container_count,
                containers,
                is_orbstack,
                daemon_version,
                daemon_started_at,
                control_port,
                control_token,
            }),
            response => Err(DaemonClientError::unexpected("status", &response)),
        }
    }

    /// Query all daemon-managed forwarded ports.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or unexpected response.
    pub async fn query_ports(&self) -> Result<Vec<ForwardedPortDetail>, DaemonClientError> {
        match self.request(&ManagementRequest::QueryPorts).await? {
            ManagementResponse::Ports { ports } => Ok(ports),
            response => Err(DaemonClientError::unexpected("ports", &response)),
        }
    }

    /// Register a container with the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or daemon-side error.
    pub async fn register_container(
        &self,
        data: ContainerRegistrationData,
    ) -> Result<String, DaemonClientError> {
        match self
            .request(&ManagementRequest::RegisterContainer(Box::new(data)))
            .await?
        {
            ManagementResponse::ContainerRegistered { container_name } => Ok(container_name),
            ManagementResponse::Error { message } => Err(DaemonClientError::Daemon { message }),
            response => Err(DaemonClientError::unexpected(
                "container_registered",
                &response,
            )),
        }
    }

    /// Deregister a container from the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or daemon-side error.
    pub async fn deregister_container(
        &self,
        container_name: impl Into<String>,
    ) -> Result<usize, DaemonClientError> {
        match self
            .request(&ManagementRequest::DeregisterContainer {
                container_name: container_name.into(),
            })
            .await?
        {
            ManagementResponse::ContainerDeregistered { ports_released, .. } => Ok(ports_released),
            ManagementResponse::Error { message } => Err(DaemonClientError::Daemon { message }),
            response => Err(DaemonClientError::unexpected(
                "container_deregistered",
                &response,
            )),
        }
    }

    /// Update a previously registered container IP.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or daemon-side error.
    pub async fn update_container_ip(
        &self,
        container_id: impl Into<String>,
        container_ip: Option<String>,
    ) -> Result<String, DaemonClientError> {
        match self
            .request(&ManagementRequest::UpdateContainerIp {
                container_id: container_id.into(),
                container_ip,
            })
            .await?
        {
            ManagementResponse::ContainerIpUpdated { container_id } => Ok(container_id),
            ManagementResponse::Error { message } => Err(DaemonClientError::Daemon { message }),
            response => Err(DaemonClientError::unexpected(
                "container_ip_updated",
                &response,
            )),
        }
    }

    /// Register an SSH-agent proxy and return bridge details.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or daemon-side error.
    pub async fn register_ssh_agent_proxy(
        &self,
        workspace: impl Into<String>,
        upstream_socket: impl Into<String>,
    ) -> Result<SshAgentProxyRegistration, DaemonClientError> {
        match self
            .request(&ManagementRequest::RegisterSshAgentProxy {
                workspace: workspace.into(),
                upstream_socket: upstream_socket.into(),
            })
            .await?
        {
            ManagementResponse::SshAgentProxyRegistered {
                bridge_port,
                refcount,
            } => Ok(SshAgentProxyRegistration {
                bridge_port,
                refcount,
            }),
            ManagementResponse::Error { message } => Err(DaemonClientError::Daemon { message }),
            response => Err(DaemonClientError::unexpected(
                "ssh_agent_proxy_registered",
                &response,
            )),
        }
    }

    /// Release one SSH-agent proxy reference.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or daemon-side error.
    pub async fn release_ssh_agent_proxy(
        &self,
        workspace: impl Into<String>,
    ) -> Result<bool, DaemonClientError> {
        match self
            .request(&ManagementRequest::ReleaseSshAgentProxy {
                workspace: workspace.into(),
            })
            .await?
        {
            ManagementResponse::SshAgentProxyReleased { torn_down } => Ok(torn_down),
            ManagementResponse::Error { message } => Err(DaemonClientError::Daemon { message }),
            response => Err(DaemonClientError::unexpected(
                "ssh_agent_proxy_released",
                &response,
            )),
        }
    }

    /// Ask the daemon to shut down.
    ///
    /// # Errors
    ///
    /// Returns an error on socket/protocol failure or daemon-side error.
    pub async fn shutdown(&self) -> Result<u32, DaemonClientError> {
        match self.request(&ManagementRequest::Shutdown).await? {
            ManagementResponse::ShuttingDown { pid } => Ok(pid),
            ManagementResponse::Error { message } => Err(DaemonClientError::Daemon { message }),
            response => Err(DaemonClientError::unexpected("shutting_down", &response)),
        }
    }
}

/// Status returned by the daemon management socket.
#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub container_count: usize,
    pub containers: Vec<ContainerSummary>,
    pub is_orbstack: bool,
    pub daemon_version: String,
    pub daemon_started_at: u64,
    pub control_port: u16,
    pub control_token: String,
}

/// Successful SSH-agent proxy registration details.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SshAgentProxyRegistration {
    pub bridge_port: u16,
    pub refcount: usize,
}

/// Error returned by daemon management client operations.
#[derive(Debug, thiserror::Error)]
pub enum DaemonClientError {
    #[error("failed to connect to management socket {socket_path}: {source}")]
    Connect {
        socket_path: PathBuf,
        source: std::io::Error,
    },
    #[error("management socket I/O error during {operation}: {source}")]
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
    #[error("management protocol error during {operation}: {source}")]
    Protocol {
        operation: &'static str,
        source: serde_json::Error,
    },
    #[error("daemon returned error: {message}")]
    Daemon { message: String },
    #[error("daemon returned unexpected response; expected {expected}, got {actual}")]
    UnexpectedResponse {
        expected: &'static str,
        actual: String,
    },
}

impl DaemonClientError {
    fn unexpected(expected: &'static str, response: &ManagementResponse) -> Self {
        Self::UnexpectedResponse {
            expected,
            actual: format!("{response:?}"),
        }
    }
}

/// Send a management request to the daemon and receive the response.
///
/// # Errors
///
/// Returns an error on connection, I/O, or protocol failure.
pub async fn send_management_request(
    socket_path: &Path,
    request: &ManagementRequest,
) -> Result<ManagementResponse, DaemonClientError> {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|source| DaemonClientError::Connect {
            socket_path: socket_path.to_path_buf(),
            source,
        })?;

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let mut json =
        serde_json::to_string(request).map_err(|source| DaemonClientError::Protocol {
            operation: "serialize request",
            source,
        })?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .await
        .map_err(|source| DaemonClientError::Io {
            operation: "write request",
            source,
        })?;
    writer
        .flush()
        .await
        .map_err(|source| DaemonClientError::Io {
            operation: "flush request",
            source,
        })?;

    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .await
        .map_err(|source| DaemonClientError::Io {
            operation: "read response",
            source,
        })?;

    serde_json::from_str(response_line.trim()).map_err(|source| DaemonClientError::Protocol {
        operation: "parse response",
        source,
    })
}
