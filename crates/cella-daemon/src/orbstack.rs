//! OrbStack-specific optimizations for port forwarding.
//!
//! When running on `OrbStack`, containers are directly accessible via
//! `container_name.orb.local` domains, eliminating the need for TCP proxies.

/// Check if the current Docker runtime is `OrbStack`.
///
/// Uses the same detection logic as `cella-env::platform::detect_runtime()`.
pub fn is_orbstack() -> bool {
    // Check DOCKER_HOST
    if let Ok(host) = std::env::var("DOCKER_HOST")
        && host.contains("orbstack")
    {
        return true;
    }

    // Check DOCKER_CONTEXT
    if let Ok(ctx) = std::env::var("DOCKER_CONTEXT")
        && ctx.to_lowercase().contains("orbstack")
    {
        return true;
    }

    // Check docker context inspect
    if let Ok(output) = std::process::Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ])
        .output()
        && output.status.success()
    {
        let endpoint = String::from_utf8_lossy(&output.stdout);
        if endpoint.to_lowercase().contains("orbstack") {
            return true;
        }
    }

    false
}

/// Generate the orb.local URL for a container port.
pub fn orb_local_url(container_name: &str, port: u16) -> String {
    format!("{container_name}.orb.local:{port}")
}

/// Whether this host reaches containers at their own address rather than
/// through the agent's reverse tunnel.
///
/// Linux shares a network namespace with the container bridge, and `OrbStack`
/// routes container addresses from the host. Everywhere else the container's
/// address is unreachable and forwards are tunnelled instead.
#[must_use]
pub const fn uses_direct_ip(is_orbstack: bool) -> bool {
    cfg!(target_os = "linux") || is_orbstack
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_ip_follows_the_runtime() {
        // OrbStack routes container addresses from the host whatever the OS.
        assert!(uses_direct_ip(true));
        // Elsewhere it depends on the host sharing the container's network.
        assert_eq!(uses_direct_ip(false), cfg!(target_os = "linux"));
    }

    #[test]
    fn orb_local_url_format() {
        let url = orb_local_url("cella-myapp-main", 3000);
        assert_eq!(url, "cella-myapp-main.orb.local:3000");
    }
}
