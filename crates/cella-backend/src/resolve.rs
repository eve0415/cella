//! Container target resolution logic.
//!
//! [`ContainerTarget`] specifies how to find a container (by ID, name, label,
//! or workspace folder) and resolves it against any [`ContainerBackend`].

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::BackendError;
use crate::traits::ContainerBackend;
use crate::types::{ContainerInfo, ContainerState};

/// Specifies how to find a target container.
pub struct ContainerTarget {
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    /// `key=value` id labels; ALL must match (AND), mirroring the official
    /// `--id-label` semantics. Empty means "no id-label targeting".
    pub id_labels: Vec<String>,
    pub workspace_folder: Option<PathBuf>,
}

impl ContainerTarget {
    /// Resolve this target to a concrete container.
    ///
    /// Resolution priority (first match wins):
    /// 1. `container_id` — direct inspect
    /// 2. `container_name` — backend resolves names via inspect
    /// 3. `id_labels` — search containers matching ALL labels (AND)
    /// 4. `workspace_folder` — search by `dev.cella.workspace_path` label
    /// 5. CWD fallback — `std::env::current_dir()` as `workspace_folder`
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ContainerNotFound` if no container matches.
    /// Returns `BackendError::ContainerNotRunning` if the container exists
    /// but is not running and `require_running` is true.
    pub async fn resolve(
        &self,
        client: &dyn ContainerBackend,
        require_running: bool,
    ) -> Result<ContainerInfo, BackendError> {
        let info = self.find(client).await?;

        if require_running && info.state != ContainerState::Running {
            return Err(BackendError::ContainerNotRunning {
                hint: format!(
                    "Container '{}' exists but is not running. Run `cella up` to start it.",
                    info.name
                ),
            });
        }

        Ok(info)
    }

    async fn find(&self, client: &dyn ContainerBackend) -> Result<ContainerInfo, BackendError> {
        if let Some(ref id) = self.container_id {
            return self.find_by_id(client, id).await;
        }
        if let Some(ref name) = self.container_name {
            return self.find_by_name(client, name).await;
        }
        if !self.id_labels.is_empty() {
            return self.find_by_labels(client, &self.id_labels).await;
        }
        self.find_by_workspace_or_cwd(client).await
    }

    async fn find_by_id(
        &self,
        client: &dyn ContainerBackend,
        id: &str,
    ) -> Result<ContainerInfo, BackendError> {
        debug!("Resolving container by ID: {id}");
        client.inspect_container(id).await
    }

    async fn find_by_name(
        &self,
        client: &dyn ContainerBackend,
        name: &str,
    ) -> Result<ContainerInfo, BackendError> {
        debug!("Resolving container by name: {name}");
        client.inspect_container(name).await
    }

    async fn find_by_workspace_or_cwd(
        &self,
        client: &dyn ContainerBackend,
    ) -> Result<ContainerInfo, BackendError> {
        let folder = self
            .workspace_folder
            .as_deref()
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| BackendError::ContainerNotFound {
                identifier: "(unable to determine current directory)".to_string(),
            })?;
        debug!(
            "Resolving container by workspace folder: {}",
            folder.display()
        );
        self.find_by_workspace(client, &folder).await
    }

    async fn find_by_labels(
        &self,
        client: &dyn ContainerBackend,
        labels: &[String],
    ) -> Result<ContainerInfo, BackendError> {
        debug!("Resolving container by labels: {}", labels.join(", "));
        // Search all runtime containers (not just cella-managed ones); every
        // label must match (AND), mirroring the official `--id-label` semantics.
        client
            .find_container_by_labels(labels)
            .await?
            .ok_or_else(|| BackendError::ContainerNotFound {
                identifier: labels.join(","),
            })
    }

    async fn find_by_workspace(
        &self,
        client: &dyn ContainerBackend,
        folder: &Path,
    ) -> Result<ContainerInfo, BackendError> {
        client
            .find_container(folder)
            .await?
            .ok_or_else(|| BackendError::ContainerNotFound {
                identifier: folder.display().to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_target_default_fields_are_none() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_labels: Vec::new(),
            workspace_folder: None,
        };
        assert!(target.container_id.is_none());
        assert!(target.container_name.is_none());
        assert!(target.id_labels.is_empty());
        assert!(target.workspace_folder.is_none());
    }

    #[test]
    fn container_target_with_id() {
        let target = ContainerTarget {
            container_id: Some("abc123".to_string()),
            container_name: None,
            id_labels: Vec::new(),
            workspace_folder: None,
        };
        assert_eq!(target.container_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn container_target_with_name() {
        let target = ContainerTarget {
            container_id: None,
            container_name: Some("my-container".to_string()),
            id_labels: Vec::new(),
            workspace_folder: None,
        };
        assert_eq!(target.container_name.as_deref(), Some("my-container"));
    }

    #[test]
    fn container_target_with_labels() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_labels: vec!["dev.cella.id=xyz".to_string(), "owner=me".to_string()],
            workspace_folder: None,
        };
        assert_eq!(target.id_labels, ["dev.cella.id=xyz", "owner=me"]);
    }

    #[test]
    fn container_target_with_workspace() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_labels: Vec::new(),
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
            id_labels: vec!["label=val".to_string()],
            workspace_folder: Some(PathBuf::from("/ws")),
        };
        assert!(target.container_id.is_some());
        assert!(target.container_name.is_some());
        assert!(!target.id_labels.is_empty());
        assert!(target.workspace_folder.is_some());
    }

    #[test]
    fn container_target_workspace_folder_with_spaces() {
        let target = ContainerTarget {
            container_id: None,
            container_name: None,
            id_labels: Vec::new(),
            workspace_folder: Some(PathBuf::from("/home/user/my project/repo")),
        };
        assert_eq!(
            target.workspace_folder.unwrap().to_string_lossy(),
            "/home/user/my project/repo"
        );
    }
}
