//! Container create/start/stop/remove/inspect operations.

use std::collections::HashMap;
use std::path::Path;

use bollard::container::{
    CreateContainerOptions as BollardCreateOptions, ListContainersOptions, LogsOptions,
    RemoveContainerOptions, StopContainerOptions,
};
use bollard::models::ContainerStateStatusEnum;
use futures_util::StreamExt;
use tracing::{debug, info};

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Container state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Stopped,
    Created,
    Removing,
    Other(String),
}

impl ContainerState {
    fn from_bollard(status: ContainerStateStatusEnum) -> Self {
        match status {
            ContainerStateStatusEnum::RUNNING => Self::Running,
            ContainerStateStatusEnum::EXITED | ContainerStateStatusEnum::DEAD => Self::Stopped,
            ContainerStateStatusEnum::CREATED => Self::Created,
            ContainerStateStatusEnum::REMOVING => Self::Removing,
            other => Self::Other(format!("{other:?}")),
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "exited" | "dead" => Self::Stopped,
            "created" => Self::Created,
            "removing" => Self::Removing,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Information about a container.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub exit_code: Option<i64>,
    pub labels: HashMap<String, String>,
    pub config_hash: Option<String>,
}

impl DockerClient {
    /// Find an existing cella container by workspace path label.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn find_container(
        &self,
        workspace_root: &Path,
    ) -> Result<Option<ContainerInfo>, CellaDockerError> {
        let canonical = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());

        let filters: HashMap<String, Vec<String>> = HashMap::from([(
            "label".to_string(),
            vec![format!("dev.cella.workspace_path={}", canonical.display())],
        )]);

        let options = ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        };

        let containers = self.inner().list_containers(Some(options)).await?;

        if let Some(summary) = containers.into_iter().next() {
            let labels = summary.labels.unwrap_or_default();
            let config_hash = labels.get("dev.cella.config_hash").cloned();
            let state = summary.state.as_deref().map_or_else(
                || ContainerState::Other("unknown".to_string()),
                ContainerState::from_str,
            );

            Ok(Some(ContainerInfo {
                id: summary.id.unwrap_or_default(),
                name: summary
                    .names
                    .and_then(|n| n.into_iter().next())
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string(),
                state,
                exit_code: None,
                labels,
                config_hash,
            }))
        } else {
            Ok(None)
        }
    }

    /// Create a container from mapped config options.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn create_container(
        &self,
        opts: &super::config_map::CreateContainerOptions,
    ) -> Result<String, CellaDockerError> {
        info!("Creating container: {}", opts.name);

        let bollard_opts = BollardCreateOptions {
            name: opts.name.as_str(),
            ..Default::default()
        };

        let config = opts.to_bollard_config();
        let response = self
            .inner()
            .create_container(Some(bollard_opts), config)
            .await?;

        for warning in response.warnings {
            tracing::warn!("Docker warning: {warning}");
        }

        debug!("Container created: {}", response.id);
        Ok(response.id)
    }

    /// Start a stopped/created container.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn start_container(&self, id: &str) -> Result<(), CellaDockerError> {
        info!("Starting container: {id}");
        self.inner().start_container::<String>(id, None).await?;
        Ok(())
    }

    /// Stop a running container.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn stop_container(&self, id: &str) -> Result<(), CellaDockerError> {
        info!("Stopping container: {id}");
        let options = StopContainerOptions { t: 10 };
        self.inner().stop_container(id, Some(options)).await?;
        Ok(())
    }

    /// Remove a container, optionally removing its volumes.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn remove_container(
        &self,
        id: &str,
        remove_volumes: bool,
    ) -> Result<(), CellaDockerError> {
        info!("Removing container: {id}");
        let options = RemoveContainerOptions {
            v: remove_volumes,
            force: false,
            ..Default::default()
        };
        self.inner().remove_container(id, Some(options)).await?;
        Ok(())
    }

    /// Get detailed container info.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn inspect_container(&self, id: &str) -> Result<ContainerInfo, CellaDockerError> {
        let inspect = self.inner().inspect_container(id, None).await?;

        let container_state = inspect.state.as_ref();
        let state = container_state.and_then(|s| s.status).map_or_else(
            || ContainerState::Other("unknown".to_string()),
            ContainerState::from_bollard,
        );
        let exit_code = container_state.and_then(|s| s.exit_code);

        let labels = inspect
            .config
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default();

        let config_hash = labels.get("dev.cella.config_hash").cloned();
        let name = inspect
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();

        Ok(ContainerInfo {
            id: inspect.id.unwrap_or_default(),
            name,
            state,
            exit_code,
            labels,
            config_hash,
        })
    }

    /// Fetch the last `tail` lines of container logs.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn container_logs(&self, id: &str, tail: u32) -> Result<String, CellaDockerError> {
        let options = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            ..Default::default()
        };

        let mut stream = self.inner().logs(id, Some(options));
        let mut output = String::new();

        while let Some(chunk) = stream.next().await {
            match chunk? {
                bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::StdErr { message } => {
                    output.push_str(&String::from_utf8_lossy(&message));
                }
                _ => {}
            }
        }

        Ok(output)
    }
}
