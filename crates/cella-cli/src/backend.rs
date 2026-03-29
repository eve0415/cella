//! Container backend selection and connection.

use cella_backend::{BackendKind, ContainerBackend};
use cella_docker::DockerClient;

/// User-selected container backend.
#[derive(Clone, clap::ValueEnum)]
pub enum BackendChoice {
    Docker,
    #[value(name = "apple-container")]
    AppleContainer,
}

impl BackendChoice {
    pub const fn to_kind(&self) -> BackendKind {
        match self {
            Self::Docker => BackendKind::Docker,
            Self::AppleContainer => BackendKind::AppleContainer,
        }
    }
}

/// Resolve the container backend from user choice, auto-detecting if needed.
///
/// When `choice` is `None`, auto-detection tries Docker first. Apple Container
/// is only attempted when Docker is unavailable and the host is macOS.
///
/// # Errors
///
/// Returns error if no backend is available.
pub fn resolve_backend(
    choice: Option<&BackendChoice>,
    docker_host: Option<&str>,
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error>> {
    match choice {
        Some(BackendChoice::Docker) => connect_docker_backend(docker_host),
        Some(BackendChoice::AppleContainer) => Err(
            "Apple Container backend is not yet available. It will be added in a future release."
                .into(),
        ),
        None => auto_detect(docker_host),
    }
}

fn auto_detect(
    docker_host: Option<&str>,
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error>> {
    // Docker is highest priority
    if let Ok(client) = connect_docker_backend(docker_host) {
        return Ok(client);
    }

    // Apple Container will be added here as lowest priority fallback

    Err(
        "No container backend available. Install Docker or another supported container runtime."
            .into(),
    )
}

fn connect_docker_backend(
    docker_host: Option<&str>,
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error>> {
    let client = docker_host.map_or_else(DockerClient::connect, |host| {
        DockerClient::connect_with_host(host)
    })?;
    Ok(Box::new(client))
}
