//! Docker socket auto-discovery for alternative container runtimes.
//!
//! When bollard's default connection (`DOCKER_HOST` / `/var/run/docker.sock`)
//! fails, this module probes known socket paths for Colima, Podman, and
//! Rancher Desktop.

use std::path::{Path, PathBuf};

use tracing::{debug, info};

/// Result of socket discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredSocket {
    /// Absolute filesystem path to the socket.
    pub path: PathBuf,
    /// How the socket was found (for diagnostics).
    pub method: DiscoveryMethod,
}

/// How a socket was discovered.
#[derive(Debug, Clone)]
pub enum DiscoveryMethod {
    /// Found via `docker context inspect`.
    DockerContext,
    /// Found by probing a known filesystem path.
    FilesystemProbe,
}

impl std::fmt::Display for DiscoveryMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DockerContext => f.write_str("docker context"),
            Self::FilesystemProbe => f.write_str("filesystem probe"),
        }
    }
}

/// Attempt to discover a Docker-compatible socket.
///
/// Strategy:
/// 1. Query `docker context inspect` for the active endpoint.
/// 2. Probe known filesystem paths for each supported runtime.
///
/// Returns `None` if no usable socket is found.
pub fn discover_socket() -> Option<DiscoveredSocket> {
    if let Some(socket) = discover_from_docker_context() {
        return Some(socket);
    }

    discover_from_known_paths()
}

/// Query `docker context inspect` for a unix socket endpoint.
fn discover_from_docker_context() -> Option<DiscoveredSocket> {
    let output = std::process::Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        debug!("docker context inspect failed, skipping context-based discovery");
        return None;
    }

    let endpoint = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let path = if let Some(stripped) = endpoint.strip_prefix("unix://") {
        PathBuf::from(stripped)
    } else if endpoint.starts_with('/') {
        PathBuf::from(&endpoint)
    } else {
        debug!("docker context endpoint is not a unix socket: {endpoint}");
        return None;
    };

    if path.exists() {
        info!(
            "Discovered Docker socket via docker context: {}",
            path.display()
        );
        Some(DiscoveredSocket {
            path,
            method: DiscoveryMethod::DockerContext,
        })
    } else {
        debug!("docker context socket does not exist: {}", path.display());
        None
    }
}

/// Probe known filesystem paths for runtime sockets.
fn discover_from_known_paths() -> Option<DiscoveredSocket> {
    let home = std::env::var("HOME").ok()?;
    let home = Path::new(&home);

    let candidates = build_candidate_paths(home);

    for path in &candidates {
        if path.exists() {
            info!("Discovered Docker socket at: {}", path.display());
            return Some(DiscoveredSocket {
                path: path.clone(),
                method: DiscoveryMethod::FilesystemProbe,
            });
        }
        debug!("Socket not found: {}", path.display());
    }

    None
}

/// Build the ordered list of candidate socket paths to probe.
fn build_candidate_paths(home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Colima default profile
    paths.push(home.join(".colima/default/docker.sock"));

    // Rancher Desktop
    paths.push(home.join(".rd/docker.sock"));

    // Podman machine sockets (macOS and Linux)
    let pattern = home
        .join(".local/share/containers/podman/machine/*/podman.sock")
        .to_string_lossy()
        .to_string();
    if let Ok(entries) = glob::glob(&pattern) {
        for entry in entries.flatten() {
            paths.push(entry);
        }
    }

    // Podman rootless socket via XDG_RUNTIME_DIR (Linux)
    #[cfg(target_os = "linux")]
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        paths.push(PathBuf::from(format!("{xdg}/podman/podman.sock")));
    }

    paths
}

/// Format an error message listing all paths that were tried.
///
/// Called when all discovery methods fail.
pub fn discovery_failure_message() -> String {
    let home_str = std::env::var("HOME").unwrap_or_else(|_| "~".into());
    let home = Path::new(&home_str);

    let mut lines = vec![
        "could not connect to a Docker-compatible runtime".to_string(),
        String::new(),
        "Checked:".to_string(),
        "  - DOCKER_HOST environment variable (not set)".to_string(),
        "  - /var/run/docker.sock".to_string(),
        "  - docker context inspect".to_string(),
    ];

    let candidates = build_candidate_paths(home);
    for path in &candidates {
        lines.push(format!("  - {}", path.display()));
    }

    lines.push(String::new());
    lines.push("Suggestions:".to_string());
    lines.push("  - Ensure your container runtime is running (Docker Desktop, Colima, Podman, or Rancher Desktop)".to_string());
    lines.push("  - Set DOCKER_HOST to point to your runtime's socket".to_string());
    lines.push("  - Use --docker-host to specify the socket path explicitly".to_string());

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_candidate_paths_includes_colima_and_rancher() {
        let home = Path::new("/home/testuser");
        let paths = build_candidate_paths(home);

        assert!(paths.contains(&PathBuf::from("/home/testuser/.colima/default/docker.sock")));
        assert!(paths.contains(&PathBuf::from("/home/testuser/.rd/docker.sock")));
    }

    #[test]
    fn discovery_failure_message_contains_suggestions() {
        let msg = discovery_failure_message();
        assert!(msg.contains("DOCKER_HOST"));
        assert!(msg.contains("--docker-host"));
        assert!(msg.contains("Colima"));
        assert!(msg.contains("Podman"));
        assert!(msg.contains("Rancher Desktop"));
    }

    #[test]
    fn discovery_method_display() {
        assert_eq!(DiscoveryMethod::DockerContext.to_string(), "docker context");
        assert_eq!(
            DiscoveryMethod::FilesystemProbe.to_string(),
            "filesystem probe"
        );
    }

    // -----------------------------------------------------------------------
    // build_candidate_paths tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_candidate_paths_at_least_two_entries() {
        let home = Path::new("/home/user");
        let paths = build_candidate_paths(home);
        // Should always have at least colima + rancher
        assert!(
            paths.len() >= 2,
            "expected at least 2 candidate paths, got {}",
            paths.len()
        );
    }

    #[test]
    fn build_candidate_paths_colima_is_first() {
        let home = Path::new("/home/user");
        let paths = build_candidate_paths(home);
        assert_eq!(
            paths[0],
            PathBuf::from("/home/user/.colima/default/docker.sock")
        );
    }

    #[test]
    fn build_candidate_paths_rancher_is_second() {
        let home = Path::new("/home/user");
        let paths = build_candidate_paths(home);
        assert_eq!(paths[1], PathBuf::from("/home/user/.rd/docker.sock"));
    }

    #[test]
    fn build_candidate_paths_uses_home_prefix() {
        let home = Path::new("/Users/developer");
        let paths = build_candidate_paths(home);
        assert!(paths[0].starts_with("/Users/developer"));
        assert!(paths[1].starts_with("/Users/developer"));
    }

    // -----------------------------------------------------------------------
    // discovery_failure_message tests
    // -----------------------------------------------------------------------

    #[test]
    fn discovery_failure_message_contains_checked_section() {
        let msg = discovery_failure_message();
        assert!(msg.contains("Checked:"));
        assert!(msg.contains("/var/run/docker.sock"));
    }

    #[test]
    fn discovery_failure_message_contains_docker_context() {
        let msg = discovery_failure_message();
        assert!(msg.contains("docker context inspect"));
    }

    #[test]
    fn discovery_failure_message_contains_suggestions_section() {
        let msg = discovery_failure_message();
        assert!(msg.contains("Suggestions:"));
    }

    #[test]
    fn discovery_failure_message_mentions_docker_host_flag() {
        let msg = discovery_failure_message();
        assert!(msg.contains("--docker-host"));
    }

    #[test]
    fn discovery_failure_message_mentions_all_runtimes() {
        let msg = discovery_failure_message();
        assert!(msg.contains("Docker Desktop"));
        assert!(msg.contains("Colima"));
        assert!(msg.contains("Podman"));
        assert!(msg.contains("Rancher Desktop"));
    }

    #[test]
    fn discovery_failure_message_starts_with_error_description() {
        let msg = discovery_failure_message();
        assert!(msg.starts_with("could not connect"));
    }

    // -----------------------------------------------------------------------
    // DiscoveredSocket tests
    // -----------------------------------------------------------------------

    #[test]
    fn discovered_socket_debug_impl() {
        let socket = DiscoveredSocket {
            path: PathBuf::from("/var/run/docker.sock"),
            method: DiscoveryMethod::FilesystemProbe,
        };
        let debug_str = format!("{socket:?}");
        assert!(debug_str.contains("docker.sock"));
        assert!(debug_str.contains("FilesystemProbe"));
    }

    #[test]
    fn discovered_socket_clone() {
        let socket = DiscoveredSocket {
            path: PathBuf::from("/tmp/test.sock"),
            method: DiscoveryMethod::DockerContext,
        };
        let cloned = socket.clone();
        assert_eq!(cloned.path, socket.path);
        assert!(matches!(cloned.method, DiscoveryMethod::DockerContext));
    }

    #[test]
    fn discovery_method_clone() {
        let method = DiscoveryMethod::FilesystemProbe;
        // Clone and verify both copies produce the same display output
        let cloned = Clone::clone(&method);
        assert_eq!(method.to_string(), cloned.to_string());
    }

    #[test]
    fn discovery_method_debug() {
        let context = DiscoveryMethod::DockerContext;
        let probe = DiscoveryMethod::FilesystemProbe;
        assert!(format!("{context:?}").contains("DockerContext"));
        assert!(format!("{probe:?}").contains("FilesystemProbe"));
    }
}
