//! Container backend selection and connection.

use cella_backend::{BackendKind, ContainerBackend};
use cella_docker::DockerClient;

/// User-selected container backend.
#[derive(Clone, clap::ValueEnum)]
pub enum BackendChoice {
    Docker,
    #[cfg(target_os = "macos")]
    #[value(name = "apple-container")]
    AppleContainer,
}

impl BackendChoice {
    pub const fn to_kind(&self) -> BackendKind {
        match self {
            Self::Docker => BackendKind::Docker,
            #[cfg(target_os = "macos")]
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
        #[cfg(target_os = "macos")]
        Some(BackendChoice::AppleContainer) => connect_apple_container_backend(),
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

    // Apple Container is lowest priority fallback (macOS only)
    #[cfg(target_os = "macos")]
    if let Ok(client) = connect_apple_container_backend() {
        return Ok(client);
    }

    Err(
        "No container backend available. Install Docker or Apple Container (macOS 26+, Apple Silicon)."
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

#[cfg(target_os = "macos")]
fn connect_apple_container_backend() -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error>>
{
    let cli = cella_container::discovery::discover()
        .ok_or("Apple Container CLI not found. Install from https://github.com/apple/container")?;

    eprintln!(
        "warning: Apple Container backend is experimental. \
         Report issues at https://github.com/eve0415/cella/issues"
    );

    Ok(Box::new(cella_container::AppleContainerBackend::new(cli)))
}
