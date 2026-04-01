//! Docker network management for cross-container communication.
//!
//! Creates and manages the `cella` bridge network that enables
//! container-to-container communication via Docker DNS.

use std::path::Path;

use bollard::Docker;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::CellaDockerError;

/// Default network name for cella containers.
pub const CELLA_NETWORK_NAME: &str = "cella";

/// Ensure the cella bridge network exists.
///
/// Creates the network if it doesn't already exist.
///
/// # Errors
///
/// Returns error if Docker API call fails.
pub async fn ensure_network(docker: &Docker) -> Result<(), CellaDockerError> {
    // Check if network already exists
    match docker.inspect_network(CELLA_NETWORK_NAME, None).await {
        Ok(_) => {
            debug!("Network '{CELLA_NETWORK_NAME}' already exists");
            return Ok(());
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            // Network doesn't exist — create it
        }
        Err(e) => return Err(e.into()),
    }

    let config = bollard::models::NetworkCreateRequest {
        name: CELLA_NETWORK_NAME.to_string(),
        driver: Some("bridge".to_string()),
        labels: Some(
            [
                ("dev.cella.tool".to_string(), "cella".to_string()),
                ("dev.cella.managed".to_string(), "true".to_string()),
            ]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    };

    docker.create_network(config).await?;
    info!("Created Docker network '{CELLA_NETWORK_NAME}'");
    Ok(())
}

/// Connect a container to the cella network.
///
/// # Errors
///
/// Returns error if the Docker API call fails.
pub async fn connect_container(
    docker: &Docker,
    container_id: &str,
) -> Result<(), CellaDockerError> {
    let config = bollard::models::NetworkConnectRequest {
        container: container_id.to_string(),
        ..Default::default()
    };

    docker.connect_network(CELLA_NETWORK_NAME, config).await?;

    debug!("Connected container {container_id} to '{CELLA_NETWORK_NAME}' network");
    Ok(())
}

/// Remove the cella network.
///
/// Only call from `cella prune --networks`. Does NOT remove on `cella down`
/// because other containers may be using the network.
///
/// # Errors
///
/// Returns error if the Docker API call fails.
pub async fn remove_network(docker: &Docker) -> Result<(), CellaDockerError> {
    match docker.remove_network(CELLA_NETWORK_NAME).await {
        Ok(()) => {
            info!("Removed Docker network '{CELLA_NETWORK_NAME}'");
            Ok(())
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            debug!("Network '{CELLA_NETWORK_NAME}' doesn't exist, nothing to remove");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Check if a container is already connected to the cella network.
///
/// # Errors
///
/// Returns error if the Docker API call fails.
pub async fn is_container_connected(
    docker: &Docker,
    container_id: &str,
) -> Result<bool, CellaDockerError> {
    let network = docker.inspect_network(CELLA_NETWORK_NAME, None).await?;

    if let Some(containers) = network.containers {
        return Ok(containers.contains_key(container_id));
    }

    Ok(false)
}

/// Ensure a container is connected to the cella network.
/// If already connected, this is a no-op.
///
/// # Errors
///
/// Returns error if the Docker API call fails.
pub async fn ensure_container_connected(
    docker: &Docker,
    container_id: &str,
) -> Result<(), CellaDockerError> {
    ensure_network(docker).await?;

    match is_container_connected(docker, container_id).await {
        Ok(true) => {
            debug!("Container {container_id} already connected to '{CELLA_NETWORK_NAME}'");
            return Ok(());
        }
        Ok(false) => {}
        Err(e) => {
            warn!("Could not check network membership: {e}");
        }
    }

    connect_container(docker, container_id).await
}

/// Derive a per-repository network name from a repo path.
///
/// Returns `cella-net-{first 12 hex chars of SHA-256 of repo_path}`.
pub fn repo_network_name(repo_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_path.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    let short = hex::encode(&hash[..6]); // 6 bytes = 12 hex chars
    format!("cella-net-{short}")
}

/// Ensure a per-repository bridge network exists and connect the container.
///
/// # Errors
///
/// Returns error if Docker API calls fail.
pub async fn ensure_repo_network(
    docker: &Docker,
    container_id: &str,
    repo_path: &Path,
) -> Result<String, CellaDockerError> {
    let net_name = repo_network_name(repo_path);

    // Check if network already exists
    match docker.inspect_network(&net_name, None).await {
        Ok(_) => {
            debug!("Repo network '{net_name}' already exists");
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            let config = bollard::models::NetworkCreateRequest {
                name: net_name.clone(),
                driver: Some("bridge".to_string()),
                labels: Some(
                    [
                        ("dev.cella.tool".to_string(), "cella".to_string()),
                        ("dev.cella.managed".to_string(), "true".to_string()),
                        (
                            "dev.cella.repo".to_string(),
                            repo_path.to_string_lossy().to_string(),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            };
            docker.create_network(config).await?;
            info!("Created repo network '{net_name}'");
        }
        Err(e) => return Err(e.into()),
    }

    // Connect container
    let connect_config = bollard::models::NetworkConnectRequest {
        container: container_id.to_string(),
        ..Default::default()
    };
    docker.connect_network(&net_name, connect_config).await?;
    debug!("Connected container {container_id} to repo network '{net_name}'");

    Ok(net_name)
}

/// Get the container's IP address on the cella network.
///
/// Returns `None` if the container is not connected to the cella network
/// or if the IP address cannot be determined.
pub async fn get_container_cella_ip(docker: &Docker, container_id: &str) -> Option<String> {
    let inspect = docker.inspect_container(container_id, None).await.ok()?;
    let networks = inspect.network_settings?.networks?;
    let cella_net = networks.get(CELLA_NETWORK_NAME)?;
    let ip = cella_net.ip_address.as_ref()?;
    if ip.is_empty() {
        return None;
    }
    Some(ip.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cella_network_name_constant() {
        assert_eq!(CELLA_NETWORK_NAME, "cella");
    }

    #[test]
    fn repo_network_name_deterministic() {
        let path = Path::new("/home/user/my-project");
        let name1 = repo_network_name(path);
        let name2 = repo_network_name(path);
        assert_eq!(name1, name2);
    }

    #[test]
    fn repo_network_name_has_prefix() {
        let name = repo_network_name(Path::new("/any/path"));
        assert!(
            name.starts_with("cella-net-"),
            "name should start with 'cella-net-': {name}"
        );
    }

    #[test]
    fn repo_network_name_has_12_hex_chars() {
        let name = repo_network_name(Path::new("/some/repo"));
        let hash_part = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(
            hash_part.len(),
            12,
            "hash portion should be 12 hex chars: {hash_part}"
        );
        assert!(
            hash_part.chars().all(|c| c.is_ascii_hexdigit()),
            "hash portion should be hex: {hash_part}"
        );
    }

    #[test]
    fn repo_network_name_different_paths_differ() {
        let name1 = repo_network_name(Path::new("/project/a"));
        let name2 = repo_network_name(Path::new("/project/b"));
        assert_ne!(name1, name2);
    }

    #[test]
    fn repo_network_name_empty_path() {
        // Edge case: empty path should still produce a valid name
        let name = repo_network_name(Path::new(""));
        assert!(name.starts_with("cella-net-"));
        let hash_part = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(hash_part.len(), 12);
    }

    // -----------------------------------------------------------------------
    // repo_network_name additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn repo_network_name_with_unicode_path() {
        let name = repo_network_name(Path::new("/home/user/proyecto-espanol"));
        assert!(name.starts_with("cella-net-"));
        let hash = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_network_name_with_spaces() {
        let name = repo_network_name(Path::new("/home/user/my project"));
        assert!(name.starts_with("cella-net-"));
        let hash = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(hash.len(), 12);
    }

    #[test]
    fn repo_network_name_with_dots() {
        let name = repo_network_name(Path::new("/home/user/.hidden-project"));
        assert!(name.starts_with("cella-net-"));
    }

    #[test]
    fn repo_network_name_very_long_path() {
        let long_path = "/".to_string() + &"a".repeat(1000);
        let name = repo_network_name(Path::new(&long_path));
        assert!(name.starts_with("cella-net-"));
        let hash = name.strip_prefix("cella-net-").unwrap();
        assert_eq!(
            hash.len(),
            12,
            "hash should always be 12 chars regardless of path length"
        );
    }

    #[test]
    fn repo_network_name_root_path() {
        let name = repo_network_name(Path::new("/"));
        assert!(name.starts_with("cella-net-"));
    }

    #[test]
    fn repo_network_name_similar_paths_differ() {
        let name1 = repo_network_name(Path::new("/project/a"));
        let name2 = repo_network_name(Path::new("/project/A"));
        assert_ne!(
            name1, name2,
            "case-different paths should produce different hashes"
        );
    }

    // -----------------------------------------------------------------------
    // Network constant test
    // -----------------------------------------------------------------------

    #[test]
    fn cella_network_name_is_valid_docker_network_name() {
        // Docker network names must be lowercase alphanumeric
        assert!(
            CELLA_NETWORK_NAME.chars().all(|c| c.is_ascii_lowercase()),
            "network name should be all lowercase"
        );
    }
}
