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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ContainerTarget construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn container_target_default_fields_are_none() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: None,
        };
        assert!(target.container_id.is_none());
        assert!(target.container_name.is_none());
        assert!(target.id_label.is_none());
        assert!(target.workspace_folder.is_none());
    }

    #[test]
    fn container_target_with_id() {
        let target = ContainerTarget {
            container_id: Some("abc123".to_string()),
            container_name: None,
            id_label: None,
            workspace_folder: None,
        };
        assert_eq!(target.container_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn container_target_with_name() {
        let target = ContainerTarget {
            container_id: None,
            container_name: Some("my-container".to_string()),
            id_label: None,
            workspace_folder: None,
        };
        assert_eq!(target.container_name.as_deref(), Some("my-container"));
    }

    #[test]
    fn container_target_with_label() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: Some("dev.cella.id=xyz".to_string()),
            workspace_folder: None,
        };
        assert_eq!(target.id_label.as_deref(), Some("dev.cella.id=xyz"));
    }

    #[test]
    fn container_target_with_workspace() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: Some(PathBuf::from("/home/user/project")),
        };
        assert_eq!(
            target.workspace_folder.as_deref(),
            Some(Path::new("/home/user/project"))
        );
    }

    #[test]
    fn container_target_all_fields_set() {
        let target = ContainerTarget {
            container_id: Some("id-1".to_string()),
            container_name: Some("name-1".to_string()),
            id_label: Some("label=val".to_string()),
            workspace_folder: Some(PathBuf::from("/ws")),
        };
        assert!(target.container_id.is_some());
        assert!(target.container_name.is_some());
        assert!(target.id_label.is_some());
        assert!(target.workspace_folder.is_some());
    }

    #[test]
    fn container_target_workspace_folder_with_spaces() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: Some(PathBuf::from("/home/user/my project/repo")),
        };
        assert_eq!(
            target.workspace_folder.unwrap().to_string_lossy(),
            "/home/user/my project/repo"
        );
    }
}
