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
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
    match choice {
        Some(BackendChoice::Docker) => connect_docker_backend(docker_host),
        #[cfg(target_os = "macos")]
        Some(BackendChoice::AppleContainer) => connect_apple_container_backend(),
        None => auto_detect(docker_host),
    }
}

fn auto_detect(
    docker_host: Option<&str>,
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
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
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
    let client = docker_host.map_or_else(DockerClient::connect, |host| {
        DockerClient::connect_with_host(host)
    })?;
    Ok(Box::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_choice_docker_to_kind() {
        let choice = BackendChoice::Docker;
        assert!(matches!(choice.to_kind(), BackendKind::Docker));
    }

    #[test]
    fn backend_choice_implements_clone() {
        let choice = BackendChoice::Docker;
        #[allow(clippy::clone_on_copy)]
        let cloned = Clone::clone(&choice);
        assert!(matches!(cloned.to_kind(), BackendKind::Docker));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn backend_choice_apple_container_to_kind() {
        let choice = BackendChoice::AppleContainer;
        assert!(matches!(choice.to_kind(), BackendKind::AppleContainer));
    }

    #[test]
    fn resolve_backend_explicit_docker() {
        // Explicit Docker choice should attempt Docker connection.
        // This will succeed or fail depending on Docker availability,
        // but the match arm is exercised either way.
        let result = resolve_backend(Some(&BackendChoice::Docker), None);
        // We only care that it doesn't panic - Docker may or may not be available
        let _ = result;
    }

    #[test]
    fn resolve_backend_auto_detect() {
        // Auto-detect with no choice should try Docker first.
        let result = resolve_backend(None, None);
        let _ = result;
    }

    #[test]
    fn resolve_backend_with_invalid_host() {
        // A clearly invalid host should fail
        let result = resolve_backend(
            Some(&BackendChoice::Docker),
            Some("tcp://invalid-host-that-does-not-exist:99999"),
        );
        // Either connect succeeds (unlikely) or fails - we just exercise the path
        let _ = result;
    }
}

#[cfg(target_os = "macos")]
fn connect_apple_container_backend()
-> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
    let cli = cella_container::discovery::discover()
        .ok_or("Apple Container CLI not found. Install from https://github.com/apple/container")?;

    eprintln!(
        "warning: Apple Container backend is experimental. \
         Report issues at https://github.com/eve0415/cella/issues"
    );

    Ok(Box::new(cella_container::AppleContainerBackend::new(cli)))
}
