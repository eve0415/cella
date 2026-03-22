//! Container create/start/stop/remove/inspect operations.

use std::collections::HashMap;
use std::path::Path;

use bollard::query_parameters::{
    CreateContainerOptions as BollardCreateOptions, ListContainersOptions, LogsOptions,
    RemoveContainerOptions, StopContainerOptions,
};
use bollard::models::ContainerStateStatusEnum;
use futures_util::StreamExt;
use tracing::{debug, info};

use crate::CellaDockerError;
use crate::client::DockerClient;
use crate::image::normalize_user;

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
    pub(crate) fn from_bollard(status: ContainerStateStatusEnum) -> Self {
        match status {
            ContainerStateStatusEnum::RUNNING => Self::Running,
            ContainerStateStatusEnum::EXITED | ContainerStateStatusEnum::DEAD => Self::Stopped,
            ContainerStateStatusEnum::CREATED => Self::Created,
            ContainerStateStatusEnum::REMOVING => Self::Removing,
            other => Self::Other(format!("{other:?}")),
        }
    }

    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "exited" | "dead" => Self::Stopped,
            "created" => Self::Created,
            "removing" => Self::Removing,
            other => Self::Other(other.to_string()),
        }
    }
}

/// A port binding exposed by the container.
#[derive(Debug, Clone)]
pub struct PortBinding {
    pub container_port: u16,
    pub host_port: Option<u16>,
    pub protocol: String,
}

/// A bind mount or volume attached to the container.
#[derive(Debug, Clone)]
pub struct MountInfo {
    pub mount_type: String,
    pub source: String,
    pub destination: String,
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
    pub ports: Vec<PortBinding>,
    pub created_at: Option<String>,
    /// The USER from the container's config (only populated via inspect, not list).
    pub container_user: Option<String>,
    /// The image used to create the container.
    pub image: Option<String>,
    /// Bind mounts and volumes (only populated via inspect, not list).
    pub mounts: Vec<MountInfo>,
}

/// Convert a bollard `ContainerSummary` into a `ContainerInfo`.
///
/// Used by both `find_container` and `list_cella_containers` to avoid
/// duplicating the field-mapping logic.
pub(crate) fn container_info_from_summary(
    summary: bollard::models::ContainerSummary,
) -> ContainerInfo {
    let labels = summary.labels.unwrap_or_default();
    let config_hash = labels.get("dev.cella.config_hash").cloned();
    let state = summary.state.as_ref().map_or_else(
        || ContainerState::Other("unknown".to_string()),
        |s| ContainerState::from_str(s.as_ref()),
    );

    let ports = summary
        .ports
        .unwrap_or_default()
        .iter()
        .map(|p| PortBinding {
            container_port: p.private_port,
            host_port: p.public_port,
            protocol: p
                .typ
                .map_or_else(|| "tcp".to_string(), |t| format!("{t:?}").to_lowercase()),
        })
        .collect();

    let created_at = summary.created.map(|ts| {
        chrono::DateTime::from_timestamp(ts, 0).map_or_else(|| ts.to_string(), |dt| dt.to_rfc3339())
    });

    ContainerInfo {
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
        ports,
        created_at,
        container_user: None,
        image: summary.image,
        mounts: Vec::new(),
    }
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
            filters: Some(filters),
            ..Default::default()
        };

        let containers = self.inner().list_containers(Some(options)).await?;

        Ok(containers
            .into_iter()
            .next()
            .map(container_info_from_summary))
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
            name: Some(opts.name.clone()),
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
        self.inner()
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await?;
        Ok(())
    }

    /// Stop a running container.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn stop_container(&self, id: &str) -> Result<(), CellaDockerError> {
        info!("Stopping container: {id}");
        let options = StopContainerOptions {
            t: Some(10),
            ..Default::default()
        };
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

        let ports = inspect
            .network_settings
            .as_ref()
            .and_then(|ns| ns.ports.as_ref())
            .map(|ports_map| {
                ports_map
                    .iter()
                    .filter_map(|(key, bindings)| {
                        let parts: Vec<&str> = key.split('/').collect();
                        let port = parts.first()?.parse::<u16>().ok()?;
                        let protocol = parts.get(1).unwrap_or(&"tcp").to_string();
                        let host_port = bindings.as_ref().and_then(|bs| {
                            bs.first()
                                .and_then(|b| b.host_port.as_ref())
                                .and_then(|hp| hp.parse::<u16>().ok())
                        });
                        Some(PortBinding {
                            container_port: port,
                            host_port,
                            protocol,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let container_user = inspect
            .config
            .as_ref()
            .and_then(|c| c.user.as_deref())
            .filter(|u| !u.is_empty())
            .map(normalize_user);

        let image = inspect.config.as_ref().and_then(|c| c.image.clone());

        let mounts = inspect
            .mounts
            .unwrap_or_default()
            .iter()
            .map(|mp| MountInfo {
                mount_type: mp
                    .typ
                    .map_or_else(|| "bind".to_string(), |t| format!("{t:?}").to_lowercase()),
                source: mp.source.clone().unwrap_or_default(),
                destination: mp.destination.clone().unwrap_or_default(),
            })
            .collect();

        Ok(ContainerInfo {
            id: inspect.id.unwrap_or_default(),
            name,
            state,
            exit_code,
            labels,
            config_hash,
            ports,
            created_at: inspect.created,
            container_user,
            image,
            mounts,
        })
    }

    /// List all cella-managed containers.
    ///
    /// Filters by the `dev.cella.workspace_path` label to find containers
    /// created by cella.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn list_cella_containers(
        &self,
        running_only: bool,
    ) -> Result<Vec<ContainerInfo>, CellaDockerError> {
        let filters: HashMap<String, Vec<String>> = HashMap::from([(
            "label".to_string(),
            vec!["dev.cella.workspace_path".to_string()],
        )]);

        let options = ListContainersOptions {
            all: !running_only,
            filters: Some(filters),
            ..Default::default()
        };

        let containers = self.inner().list_containers(Some(options)).await?;

        Ok(containers
            .into_iter()
            .map(container_info_from_summary)
            .collect())
    }

    /// Fetch the last `tail` lines of container logs.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn container_logs(&self, id: &str, tail: u32) -> Result<String, CellaDockerError> {
        let options = LogsOptions {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use bollard::models::{
        ContainerSummary, ContainerSummaryStateEnum, PortSummary, PortSummaryTypeEnum,
    };

    use super::*;
    use crate::client::DockerApi;
    use crate::client::mock::{MockCall, MockDockerClient};

    // -----------------------------------------------------------------------
    // ContainerState::from_str tests
    // -----------------------------------------------------------------------

    #[test]
    fn from_str_running() {
        assert_eq!(ContainerState::from_str("running"), ContainerState::Running);
    }

    #[test]
    fn from_str_exited() {
        assert_eq!(ContainerState::from_str("exited"), ContainerState::Stopped);
    }

    #[test]
    fn from_str_dead() {
        assert_eq!(ContainerState::from_str("dead"), ContainerState::Stopped);
    }

    #[test]
    fn from_str_created() {
        assert_eq!(ContainerState::from_str("created"), ContainerState::Created);
    }

    #[test]
    fn from_str_removing() {
        assert_eq!(
            ContainerState::from_str("removing"),
            ContainerState::Removing
        );
    }

    #[test]
    fn from_str_unknown() {
        assert_eq!(
            ContainerState::from_str("unknown_state"),
            ContainerState::Other("unknown_state".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // ContainerState::from_bollard tests
    // -----------------------------------------------------------------------

    #[test]
    fn from_bollard_running() {
        assert_eq!(
            ContainerState::from_bollard(ContainerStateStatusEnum::RUNNING),
            ContainerState::Running
        );
    }

    #[test]
    fn from_bollard_exited() {
        assert_eq!(
            ContainerState::from_bollard(ContainerStateStatusEnum::EXITED),
            ContainerState::Stopped
        );
    }

    #[test]
    fn from_bollard_dead() {
        assert_eq!(
            ContainerState::from_bollard(ContainerStateStatusEnum::DEAD),
            ContainerState::Stopped
        );
    }

    #[test]
    fn from_bollard_created() {
        assert_eq!(
            ContainerState::from_bollard(ContainerStateStatusEnum::CREATED),
            ContainerState::Created
        );
    }

    #[test]
    fn from_bollard_removing() {
        assert_eq!(
            ContainerState::from_bollard(ContainerStateStatusEnum::REMOVING),
            ContainerState::Removing
        );
    }

    // -----------------------------------------------------------------------
    // container_info_from_summary tests
    // -----------------------------------------------------------------------

    fn make_summary(
        id: Option<&str>,
        names: Option<Vec<&str>>,
        image: Option<&str>,
        state: Option<ContainerSummaryStateEnum>,
        labels: Option<HashMap<String, String>>,
        ports: Option<Vec<PortSummary>>,
        created: Option<i64>,
    ) -> ContainerSummary {
        ContainerSummary {
            id: id.map(String::from),
            names: names.map(|n| n.into_iter().map(String::from).collect()),
            image: image.map(String::from),
            state,
            labels,
            ports,
            created,
            ..Default::default()
        }
    }

    #[test]
    fn summary_basic_field_extraction() {
        let mut labels = HashMap::new();
        labels.insert("key".to_string(), "value".to_string());

        let summary = make_summary(
            Some("abc123"),
            Some(vec!["/my-container"]),
            Some("ubuntu:22.04"),
            Some(ContainerSummaryStateEnum::RUNNING),
            Some(labels.clone()),
            None,
            None,
        );

        let info = container_info_from_summary(summary);
        assert_eq!(info.id, "abc123");
        assert_eq!(info.name, "my-container");
        assert_eq!(info.state, ContainerState::Running);
        assert_eq!(info.image.as_deref(), Some("ubuntu:22.04"));
        assert_eq!(info.labels.get("key").map(String::as_str), Some("value"));
    }

    #[test]
    fn summary_name_strips_leading_slash() {
        let summary = make_summary(
            Some("id1"),
            Some(vec!["/slash-name"]),
            None,
            Some(ContainerSummaryStateEnum::RUNNING),
            None,
            None,
            None,
        );
        let info = container_info_from_summary(summary);
        assert_eq!(info.name, "slash-name");
    }

    #[test]
    fn summary_config_hash_from_labels() {
        let mut labels = HashMap::new();
        labels.insert("dev.cella.config_hash".to_string(), "deadbeef".to_string());

        let summary = make_summary(
            Some("id2"),
            Some(vec!["/test"]),
            None,
            Some(ContainerSummaryStateEnum::RUNNING),
            Some(labels),
            None,
            None,
        );

        let info = container_info_from_summary(summary);
        assert_eq!(info.config_hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn summary_ports_mapping() {
        let ports = vec![PortSummary {
            private_port: 8080,
            public_port: Some(3000),
            typ: Some(PortSummaryTypeEnum::TCP),
            ip: None,
        }];

        let summary = make_summary(
            Some("id3"),
            Some(vec!["/web"]),
            None,
            Some(ContainerSummaryStateEnum::RUNNING),
            None,
            Some(ports),
            None,
        );

        let info = container_info_from_summary(summary);
        assert_eq!(info.ports.len(), 1);
        assert_eq!(info.ports[0].container_port, 8080);
        assert_eq!(info.ports[0].host_port, Some(3000));
        assert_eq!(info.ports[0].protocol, "tcp");
    }

    #[test]
    fn summary_port_udp_protocol() {
        let ports = vec![PortSummary {
            private_port: 53,
            public_port: None,
            typ: Some(PortSummaryTypeEnum::UDP),
            ip: None,
        }];

        let summary = make_summary(
            Some("id4"),
            None,
            None,
            Some(ContainerSummaryStateEnum::RUNNING),
            None,
            Some(ports),
            None,
        );

        let info = container_info_from_summary(summary);
        assert_eq!(info.ports[0].protocol, "udp");
        assert_eq!(info.ports[0].host_port, None);
    }

    #[test]
    fn summary_port_no_type_defaults_tcp() {
        let ports = vec![PortSummary {
            private_port: 443,
            public_port: Some(8443),
            typ: None,
            ip: None,
        }];

        let summary = make_summary(
            Some("id5"),
            None,
            None,
            Some(ContainerSummaryStateEnum::RUNNING),
            None,
            Some(ports),
            None,
        );

        let info = container_info_from_summary(summary);
        assert_eq!(info.ports[0].protocol, "tcp");
    }

    #[test]
    fn summary_no_ports() {
        let summary = make_summary(Some("id6"), None, None, Some(ContainerSummaryStateEnum::RUNNING), None, None, None);
        let info = container_info_from_summary(summary);
        assert!(info.ports.is_empty());
    }

    #[test]
    fn summary_no_labels() {
        let summary = make_summary(None, None, None, Some(ContainerSummaryStateEnum::RUNNING), None, None, None);
        let info = container_info_from_summary(summary);
        assert!(info.labels.is_empty());
        assert!(info.config_hash.is_none());
    }

    #[test]
    fn summary_no_state_becomes_other_unknown() {
        let summary = make_summary(Some("id7"), None, None, None, None, None, None);
        let info = container_info_from_summary(summary);
        assert_eq!(info.state, ContainerState::Other("unknown".to_string()));
    }

    #[test]
    fn summary_created_at_timestamp_to_rfc3339() {
        // 2024-01-15T00:00:00+00:00 = 1705276800
        let summary = make_summary(
            Some("id8"),
            None,
            None,
            Some(ContainerSummaryStateEnum::RUNNING),
            None,
            None,
            Some(1_705_276_800),
        );
        let info = container_info_from_summary(summary);
        let created = info.created_at.expect("should have created_at");
        assert!(
            created.contains("2024-01-15"),
            "expected RFC3339 date, got: {created}"
        );
    }

    #[test]
    fn summary_no_names() {
        let summary = make_summary(Some("id9"), None, None, Some(ContainerSummaryStateEnum::EXITED), None, None, None);
        let info = container_info_from_summary(summary);
        assert_eq!(info.name, "");
    }

    // -----------------------------------------------------------------------
    // Mock-based operation tests
    // -----------------------------------------------------------------------

    fn sample_container_info(id: &str, name: &str, state: ContainerState) -> ContainerInfo {
        ContainerInfo {
            id: id.to_string(),
            name: name.to_string(),
            state,
            exit_code: None,
            labels: HashMap::new(),
            config_hash: None,
            ports: Vec::new(),
            created_at: None,
            container_user: None,
            image: None,
            mounts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn find_container_returns_none_when_no_match() {
        let mock = MockDockerClient::new();
        mock.find_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(None));

        let result = mock.find_container(Path::new("/tmp/project")).await;
        assert!(result.unwrap().is_none());

        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], MockCall::FindContainer { .. }));
    }

    #[tokio::test]
    async fn find_container_returns_some_with_correct_fields() {
        let mock = MockDockerClient::new();
        let info = sample_container_info("abc123", "dev-container", ContainerState::Running);
        mock.find_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(Some(info)));

        let result = mock
            .find_container(Path::new("/home/user/project"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.id, "abc123");
        assert_eq!(result.name, "dev-container");
        assert_eq!(result.state, ContainerState::Running);
    }

    #[tokio::test]
    async fn inspect_container_returns_detailed_info() {
        let mock = MockDockerClient::new();
        let mut info = sample_container_info("inspect-id", "inspected", ContainerState::Running);
        info.exit_code = Some(0);
        info.container_user = Some("vscode".to_string());
        info.image = Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string());

        mock.inspect_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(info));

        let result = mock.inspect_container("inspect-id").await.unwrap();
        assert_eq!(result.id, "inspect-id");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.container_user.as_deref(), Some("vscode"));
        assert_eq!(
            result.image.as_deref(),
            Some("mcr.microsoft.com/devcontainers/base:ubuntu")
        );
    }

    #[tokio::test]
    async fn list_cella_containers_returns_empty_vec() {
        let mock = MockDockerClient::new();
        mock.list_cella_containers_responses
            .lock()
            .unwrap()
            .push_back(Ok(Vec::new()));

        let result = mock.list_cella_containers(false).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn list_cella_containers_returns_multiple() {
        let mock = MockDockerClient::new();
        let containers = vec![
            sample_container_info("c1", "first", ContainerState::Running),
            sample_container_info("c2", "second", ContainerState::Stopped),
            sample_container_info("c3", "third", ContainerState::Created),
        ];
        mock.list_cella_containers_responses
            .lock()
            .unwrap()
            .push_back(Ok(containers));

        let result = mock.list_cella_containers(false).await.unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "first");
        assert_eq!(result[1].state, ContainerState::Stopped);
        assert_eq!(result[2].id, "c3");
    }

    #[tokio::test]
    async fn list_cella_containers_running_only_records_flag() {
        let mock = MockDockerClient::new();
        mock.list_cella_containers_responses
            .lock()
            .unwrap()
            .push_back(Ok(Vec::new()));

        let _ = mock.list_cella_containers(true).await;

        let calls = mock.get_calls();
        assert!(matches!(
            calls[0],
            MockCall::ListCellaContainers { running_only: true }
        ));
    }

    #[tokio::test]
    async fn start_container_records_correct_call() {
        let mock = MockDockerClient::new();
        mock.start_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(()));

        mock.start_container("container-42").await.unwrap();

        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::StartContainer { id } => assert_eq!(id, "container-42"),
            other => panic!("expected StartContainer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_container_propagates_docker_error() {
        let mock = MockDockerClient::new();
        mock.start_container_responses
            .lock()
            .unwrap()
            .push_back(Err(CellaDockerError::DockerApi(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 500,
                    message: "bind mount source does not exist".to_string(),
                },
            )));
        let result = mock.start_container("test-id").await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("bind mount source"));
    }

    #[tokio::test]
    async fn stop_container_records_correct_call() {
        let mock = MockDockerClient::new();
        mock.stop_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(()));

        mock.stop_container("stop-me").await.unwrap();

        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::StopContainer { id } => assert_eq!(id, "stop-me"),
            other => panic!("expected StopContainer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_container_records_call_with_volumes_flag() {
        let mock = MockDockerClient::new();
        mock.remove_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(()));

        mock.remove_container("rm-me", true).await.unwrap();

        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            MockCall::RemoveContainer { id, remove_volumes } => {
                assert_eq!(id, "rm-me");
                assert!(remove_volumes);
            }
            other => panic!("expected RemoveContainer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_container_without_volumes() {
        let mock = MockDockerClient::new();
        mock.remove_container_responses
            .lock()
            .unwrap()
            .push_back(Ok(()));

        mock.remove_container("rm-no-vol", false).await.unwrap();

        let calls = mock.get_calls();
        match &calls[0] {
            MockCall::RemoveContainer { id, remove_volumes } => {
                assert_eq!(id, "rm-no-vol");
                assert!(!remove_volumes);
            }
            other => panic!("expected RemoveContainer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn container_logs_returns_log_output() {
        let mock = MockDockerClient::new();
        mock.container_logs_responses
            .lock()
            .unwrap()
            .push_back(Ok("line 1\nline 2\n".to_string()));

        let logs = mock.container_logs("log-container", 50).await.unwrap();
        assert_eq!(logs, "line 1\nline 2\n");

        let calls = mock.get_calls();
        match &calls[0] {
            MockCall::ContainerLogs { id, tail } => {
                assert_eq!(id, "log-container");
                assert_eq!(*tail, 50);
            }
            other => panic!("expected ContainerLogs, got {other:?}"),
        }
    }
}
