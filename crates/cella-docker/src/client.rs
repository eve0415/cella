//! Docker daemon connection management.

use bollard::Docker;
use tracing::debug;

use crate::CellaDockerError;

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

    /// Access the inner bollard client (for module-internal use).
    pub(crate) const fn inner(&self) -> &Docker {
        &self.inner
    }
}
