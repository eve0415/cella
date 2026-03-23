//! Shared container resolution logic for exec, shell, list, and down.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bollard::query_parameters::ListContainersOptions;
use tracing::debug;

use crate::client::DockerClient;
use crate::container::{ContainerInfo, ContainerState};
use crate::error::CellaDockerError;

/// Specifies how to find a target container.
pub struct ContainerTarget {
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub id_label: Option<String>,
    pub workspace_folder: Option<PathBuf>,
}

impl ContainerTarget {
    /// Resolve this target to a concrete container.
    ///
    /// Resolution priority (first match wins):
    /// 1. `container_id` — direct inspect
    /// 2. `container_name` — Docker resolves names via inspect
    /// 3. `id_label` — query by arbitrary label filter
    /// 4. `workspace_folder` — query by `dev.cella.workspace_path` label
    /// 5. CWD fallback — `std::env::current_dir()` as `workspace_folder`
    ///
    /// If `require_running` is true, returns an error when the container exists
    /// but is not running, with a helpful hint.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::ContainerNotFound` if no container matches.
    /// Returns `CellaDockerError::ContainerNotRunning` if the container exists
    /// but is not running and `require_running` is true.
    pub async fn resolve(
        &self,
        client: &DockerClient,
        require_running: bool,
    ) -> Result<ContainerInfo, CellaDockerError> {
        let info = self.find(client).await?;

        if require_running && info.state != ContainerState::Running {
            return Err(CellaDockerError::ContainerNotRunning {
                hint: format!(
                    "Container '{}' exists but is not running. Run `cella up` to start it.",
                    info.name
                ),
            });
        }

        Ok(info)
    }

    async fn find(&self, client: &DockerClient) -> Result<ContainerInfo, CellaDockerError> {
        if let Some(ref id) = self.container_id {
            return self.find_by_id(client, id).await;
        }
        if let Some(ref name) = self.container_name {
            return self.find_by_name(client, name).await;
        }
        if let Some(ref label) = self.id_label {
            return self.find_by_label(client, label).await;
        }
        self.find_by_workspace_or_cwd(client).await
    }

    async fn find_by_id(
        &self,
        client: &DockerClient,
        id: &str,
    ) -> Result<ContainerInfo, CellaDockerError> {
        debug!("Resolving container by ID: {id}");
        client.inspect_container(id).await
    }

    async fn find_by_name(
        &self,
        client: &DockerClient,
        name: &str,
    ) -> Result<ContainerInfo, CellaDockerError> {
        debug!("Resolving container by name: {name}");
        client.inspect_container(name).await
    }

    async fn find_by_workspace_or_cwd(
        &self,
        client: &DockerClient,
    ) -> Result<ContainerInfo, CellaDockerError> {
        let folder = self
            .workspace_folder
            .as_deref()
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| CellaDockerError::ContainerNotFound {
                workspace: "(unable to determine current directory)".to_string(),
            })?;
        debug!(
            "Resolving container by workspace folder: {}",
            folder.display()
        );
        self.find_by_workspace(client, &folder).await
    }

    async fn find_by_label(
        &self,
        client: &DockerClient,
        label: &str,
    ) -> Result<ContainerInfo, CellaDockerError> {
        let filters: HashMap<String, Vec<String>> =
            HashMap::from([("label".to_string(), vec![label.to_string()])]);

        let options = ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        };

        let containers = client.inner().list_containers(Some(options)).await?;

        if let Some(summary) = containers.into_iter().next() {
            let id = summary.id.as_deref().unwrap_or_default();
            client.inspect_container(id).await
        } else {
            Err(CellaDockerError::ContainerNotFound {
                workspace: format!("label={label}"),
            })
        }
    }

    async fn find_by_workspace(
        &self,
        client: &DockerClient,
        folder: &Path,
    ) -> Result<ContainerInfo, CellaDockerError> {
        client
            .find_container(folder)
            .await?
            .ok_or_else(|| CellaDockerError::ContainerNotFound {
                workspace: folder.display().to_string(),
            })
    }
}
