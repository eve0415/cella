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

/// Shared CLI flags for backend selection.
#[derive(clap::Args, Clone, Default)]
pub struct BackendArgs {
    /// Container backend to use (auto-detected if not specified).
    #[arg(long, value_enum)]
    pub backend: Option<BackendChoice>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    pub docker_host: Option<String>,
}

impl BackendArgs {
    /// Resolve the container backend, validating flag combinations.
    ///
    /// # Errors
    ///
    /// Returns error if `--backend apple-container` is combined with
    /// `--docker-host`, or if no backend is available.
    pub async fn resolve(
        &self,
    ) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(target_os = "macos")]
        if matches!(self.backend, Some(BackendChoice::AppleContainer)) && self.docker_host.is_some()
        {
            return Err(
                "--docker-host cannot be used with --backend apple-container; \
                 --docker-host is Docker-specific"
                    .into(),
            );
        }
        resolve_backend(self.backend.as_ref(), self.docker_host.as_deref()).await
    }

    /// Convenience wrapper that coerces the error to `Box<dyn Error>`.
    ///
    /// # Errors
    ///
    /// Returns error if backend resolution fails (see [`resolve`](Self::resolve)).
    pub async fn resolve_client(
        &self,
    ) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error>> {
        self.resolve()
            .await
            .map_err(|e| e as Box<dyn std::error::Error>)
    }
}

/// Resolve the container backend from user choice, auto-detecting if needed.
///
/// When `choice` is `None`, auto-detection tries Docker first and pings to
/// verify the daemon is responsive. Apple Container is only attempted when
/// Docker is unavailable and the host is macOS.
///
/// # Errors
///
/// Returns error if no backend is available.
pub async fn resolve_backend(
    choice: Option<&BackendChoice>,
    docker_host: Option<&str>,
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
    match choice {
        Some(BackendChoice::Docker) => connect_docker_backend(docker_host),
        #[cfg(target_os = "macos")]
        Some(BackendChoice::AppleContainer) => connect_apple_container_backend(),
        None => auto_detect(docker_host).await,
    }
}

async fn auto_detect(
    docker_host: Option<&str>,
) -> Result<Box<dyn ContainerBackend>, Box<dyn std::error::Error + Send + Sync>> {
    // Docker is highest priority.
    match connect_docker_backend(docker_host) {
        Ok(client) => {
            // Verify the daemon is actually responding, not just that a socket
            // exists. If Docker is installed but stopped, fall through to Apple
            // Container instead of failing with a confusing error later.
            match client.ping().await {
                Ok(()) => return Ok(client),
                Err(e) => {
                    if docker_host.is_some() {
                        return Err(e.into());
                    }
                    // Socket exists but daemon not responding — try next backend
                }
            }
        }
        Err(e) => {
            // If the user explicitly targeted a Docker host, don't silently
            // fall back to a different backend — fail with the Docker error.
            if docker_host.is_some() {
                return Err(e);
            }
        }
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

    #[tokio::test]
    async fn resolve_backend_explicit_docker() {
        // Explicit Docker choice should attempt Docker connection.
        // This will succeed or fail depending on Docker availability,
        // but the match arm is exercised either way.
        let result = resolve_backend(Some(&BackendChoice::Docker), None).await;
        // We only care that it doesn't panic - Docker may or may not be available
        let _ = result;
    }

    #[tokio::test]
    async fn resolve_backend_auto_detect() {
        // Auto-detect with no choice should try Docker first.
        let result = resolve_backend(None, None).await;
        let _ = result;
    }

    #[tokio::test]
    async fn resolve_backend_with_invalid_host() {
        // A clearly invalid host should fail
        let result = resolve_backend(
            Some(&BackendChoice::Docker),
            Some("tcp://invalid-host-that-does-not-exist:99999"),
        )
        .await;
        // Either connect succeeds (unlikely) or fails - we just exercise the path
        let _ = result;
    }

    #[tokio::test]
    async fn backend_args_resolve_exercises_path() {
        let args = BackendArgs::default();
        // Auto-detect: exercises the full path regardless of Docker availability
        let _ = args.resolve().await;
    }
}
