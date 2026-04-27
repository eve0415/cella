//! Docker network management for cross-container communication.
//!
//! Creates and manages the `cella` bridge network that enables
//! container-to-container communication via Docker DNS.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bollard::Docker;
use cella_backend::{ManagedNetwork, RemovalOutcome};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::CellaDockerError;

/// Label that marks a Docker network as cella-managed.
const MANAGED_LABEL: &str = "dev.cella.managed";
/// Label value required on `MANAGED_LABEL` for cella-managed networks.
const MANAGED_VALUE: &str = "true";
/// Label that carries the workspace path on per-repo networks.
const REPO_LABEL: &str = "dev.cella.repo";

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
    connect_container_to_named_network(docker, container_id, CELLA_NETWORK_NAME).await
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

/// Canonicalize a path for network identity, falling back to the
/// original path if canonicalization fails (e.g. path doesn't exist).
///
/// Must be applied consistently at every network-op boundary so that
/// `cella down --rm` and `cella prune` hash to the same name the
/// container was created with. `container_labels` already canonicalizes
/// the `dev.cella.workspace_path` label, so this keeps network names
/// aligned with that source of truth.
fn canonicalize_for_network(repo_path: &Path) -> PathBuf {
    repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf())
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
    let canonical = canonicalize_for_network(repo_path);
    let net_name = repo_network_name(&canonical);

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
                        (MANAGED_LABEL.to_string(), MANAGED_VALUE.to_string()),
                        (
                            REPO_LABEL.to_string(),
                            canonical.to_string_lossy().to_string(),
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

/// Derive the workspace network name the same way `cella up` does, so
/// `cella down --rm` / `cella prune` target the matching network.
pub fn workspace_network_name(workspace_root: &Path) -> String {
    repo_network_name(&canonicalize_for_network(workspace_root))
}

/// Return `true` when a network's labels + endpoint count indicate it's
/// a cella-managed orphan (safe to remove).
fn is_orphan(labels: &HashMap<String, String>, container_count: usize) -> bool {
    labels.get(MANAGED_LABEL).map(String::as_str) == Some(MANAGED_VALUE) && container_count == 0
}

/// List every Docker network labeled `dev.cella.managed=true` with its
/// current endpoint count.
///
/// Issues one `list_networks` call plus one `inspect_network` per match
/// (endpoint counts only come from inspect). Networks that vanish between
/// list and inspect are silently skipped.
///
/// # Errors
///
/// Returns error if the Docker `list_networks` call fails.
pub async fn list_managed_networks(
    docker: &Docker,
) -> Result<Vec<ManagedNetwork>, CellaDockerError> {
    let filters: HashMap<String, Vec<String>> = HashMap::from([(
        "label".to_string(),
        vec![format!("{MANAGED_LABEL}={MANAGED_VALUE}")],
    )]);
    let options = bollard::query_parameters::ListNetworksOptions {
        filters: Some(filters),
    };
    let networks = docker.list_networks(Some(options)).await?;

    let mut out = Vec::with_capacity(networks.len());
    for net in networks {
        let Some(name) = net.name else { continue };

        // list_networks doesn't include endpoint counts, so inspect each.
        let inspected = match docker.inspect_network(&name, None).await {
            Ok(inspect) => inspect,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => continue,
            Err(e) => return Err(e.into()),
        };

        let labels = inspected.labels.unwrap_or_default();
        let container_count = inspected.containers.as_ref().map_or(0, HashMap::len);
        let repo_path = labels.get(REPO_LABEL).cloned();
        let created_at = inspected.created.as_ref().map(ToString::to_string);

        out.push(ManagedNetwork {
            name,
            repo_path,
            container_count,
            created_at,
            labels,
        });
    }
    Ok(out)
}

/// Remove `name` if it's cella-managed AND has zero attached containers.
///
/// Never force-disconnects endpoints. A 404 on either inspect or remove
/// is treated as success (already gone). Networks missing the
/// `dev.cella.managed=true` label are treated as "in use" from the
/// caller's perspective: we refuse to touch them and return
/// `SkippedInUse`.
///
/// # Errors
///
/// Returns error for any Docker API error other than 404 during inspect
/// or remove.
pub async fn remove_network_if_orphan(
    docker: &Docker,
    name: &str,
) -> Result<RemovalOutcome, CellaDockerError> {
    let inspect = match docker.inspect_network(name, None).await {
        Ok(n) => n,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(RemovalOutcome::NotFound),
        Err(e) => return Err(e.into()),
    };

    let labels = inspect.labels.unwrap_or_default();
    let container_count = inspect.containers.as_ref().map_or(0, HashMap::len);

    if !is_orphan(&labels, container_count) {
        debug!(
            network = name,
            container_count, "network not an orphan, leaving in place"
        );
        return Ok(RemovalOutcome::SkippedInUse);
    }

    match docker.remove_network(name).await {
        Ok(()) => {
            info!("Removed Docker network '{name}'");
            Ok(RemovalOutcome::Removed)
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(RemovalOutcome::NotFound),
        Err(e) => Err(e.into()),
    }
}

/// Connect a container to an arbitrary named network.
///
/// # Errors
///
/// Returns error if the Docker API call fails.
pub async fn connect_container_to_named_network(
    docker: &Docker,
    container_id: &str,
    network_name: &str,
) -> Result<(), CellaDockerError> {
    let config = bollard::models::NetworkConnectRequest {
        container: container_id.to_string(),
        ..Default::default()
    };
    docker.connect_network(network_name, config).await?;
    debug!("Connected container {container_id} to network '{network_name}'");
    Ok(())
}

/// Check whether a named Docker network exists.
///
/// # Errors
///
/// Returns error if the Docker API call fails (other than 404).
pub async fn named_network_exists(
    docker: &Docker,
    network_name: &str,
) -> Result<bool, CellaDockerError> {
    match docker.inspect_network(network_name, None).await {
        Ok(_) => Ok(true),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn pick_container_ip(
    networks: &HashMap<String, bollard::models::EndpointSettings>,
) -> Option<String> {
    if let Some(ip) = networks
        .get(CELLA_NETWORK_NAME)
        .and_then(|n| n.ip_address.as_ref())
        .filter(|ip| !ip.is_empty())
    {
        return Some(ip.clone());
    }

    for (name, endpoint) in networks {
        if let Some(ip) = endpoint.ip_address.as_ref().filter(|ip| !ip.is_empty()) {
            debug!("Container not on '{CELLA_NETWORK_NAME}' network, using IP {ip} from '{name}'");
            return Some(ip.clone());
        }
    }

    None
}

/// Get the container's IP address, preferring the cella network.
///
/// Returns `None` if the container has no IP on any Docker network.
pub async fn get_container_ip(docker: &Docker, container_id: &str) -> Option<String> {
    let inspect = docker.inspect_container(container_id, None).await.ok()?;
    let networks = inspect.network_settings?.networks?;
    pick_container_ip(&networks)
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

    // -----------------------------------------------------------------------
    // Orphan predicate tests
    // -----------------------------------------------------------------------

    fn labels_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn is_orphan_requires_managed_label() {
        let labels = labels_from(&[("dev.cella.tool", "cella")]);
        assert!(!is_orphan(&labels, 0));
    }

    #[test]
    fn is_orphan_requires_managed_label_value_true() {
        let labels = labels_from(&[(MANAGED_LABEL, "false")]);
        assert!(!is_orphan(&labels, 0));
    }

    #[test]
    fn is_orphan_managed_label_is_case_sensitive() {
        let labels = labels_from(&[(MANAGED_LABEL, "True")]);
        assert!(
            !is_orphan(&labels, 0),
            "label value comparison must be exact: 'True' != 'true'"
        );
    }

    #[test]
    fn is_orphan_requires_zero_containers() {
        let labels = labels_from(&[(MANAGED_LABEL, MANAGED_VALUE)]);
        assert!(!is_orphan(&labels, 1));
        assert!(!is_orphan(&labels, 5));
    }

    #[test]
    fn is_orphan_managed_and_empty_is_orphan() {
        let labels = labels_from(&[(MANAGED_LABEL, MANAGED_VALUE)]);
        assert!(is_orphan(&labels, 0));
    }

    #[test]
    fn is_orphan_managed_with_repo_label_and_empty_is_orphan() {
        let labels = labels_from(&[
            (MANAGED_LABEL, MANAGED_VALUE),
            (REPO_LABEL, "/home/user/foo"),
        ]);
        assert!(is_orphan(&labels, 0));
    }

    #[test]
    fn is_orphan_empty_labels_is_not_orphan() {
        let labels = HashMap::new();
        assert!(!is_orphan(&labels, 0));
    }

    // -----------------------------------------------------------------------
    // pick_container_ip tests
    // -----------------------------------------------------------------------

    #[test]
    fn pick_ip_prefers_cella_network() {
        let mut networks = HashMap::new();
        networks.insert(
            CELLA_NETWORK_NAME.to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.18.0.2".to_string()),
                ..Default::default()
            },
        );
        networks.insert(
            "compose_default".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.19.0.3".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(pick_container_ip(&networks), Some("172.18.0.2".to_string()));
    }

    #[test]
    fn pick_ip_falls_back_to_other_network() {
        let mut networks = HashMap::new();
        networks.insert(
            "myproject_default".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.20.0.5".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(pick_container_ip(&networks), Some("172.20.0.5".to_string()));
    }

    #[test]
    fn pick_ip_skips_empty_addresses() {
        let mut networks = HashMap::new();
        networks.insert(
            "net1".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some(String::new()),
                ..Default::default()
            },
        );
        networks.insert(
            "net2".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.21.0.2".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(pick_container_ip(&networks), Some("172.21.0.2".to_string()));
    }

    #[test]
    fn pick_ip_falls_back_when_cella_ip_is_empty() {
        let mut networks = HashMap::new();
        networks.insert(
            CELLA_NETWORK_NAME.to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some(String::new()),
                ..Default::default()
            },
        );
        networks.insert(
            "compose_default".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.19.0.3".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(pick_container_ip(&networks), Some("172.19.0.3".to_string()));
    }

    #[test]
    fn pick_ip_falls_back_when_cella_ip_is_none() {
        let mut networks = HashMap::new();
        networks.insert(
            CELLA_NETWORK_NAME.to_string(),
            bollard::models::EndpointSettings {
                ip_address: None,
                ..Default::default()
            },
        );
        networks.insert(
            "compose_default".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.19.0.4".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(pick_container_ip(&networks), Some("172.19.0.4".to_string()));
    }

    #[test]
    fn pick_ip_returns_none_when_all_ips_empty() {
        let mut networks = HashMap::new();
        networks.insert(
            "net1".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some(String::new()),
                ..Default::default()
            },
        );
        networks.insert(
            "net2".to_string(),
            bollard::models::EndpointSettings {
                ip_address: None,
                ..Default::default()
            },
        );
        assert_eq!(pick_container_ip(&networks), None);
    }

    #[test]
    fn pick_ip_returns_none_when_no_networks() {
        let networks = HashMap::new();
        assert_eq!(pick_container_ip(&networks), None);
    }
}
