//! Docker runtime detection for platform-specific SSH agent forwarding.

use std::path::Path;

/// Detected Docker runtime environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerRuntime {
    /// Docker Desktop (macOS/Windows) — provides SSH agent via VM.
    DockerDesktop,
    /// `OrbStack` (macOS) — direct socket bind-mount works.
    OrbStack,
    /// Native Docker on Linux — direct socket bind-mount works.
    LinuxNative,
    /// Colima (macOS) — direct socket bind-mount works.
    Colima,
    /// Unknown runtime — try direct bind-mount with warning.
    Unknown,
}

/// Detect the Docker runtime from environment variables and platform hints.
pub fn detect_runtime() -> DockerRuntime {
    // Check DOCKER_HOST for runtime-specific patterns
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if host.contains("orbstack") {
            return DockerRuntime::OrbStack;
        }
        if host.contains("colima") {
            return DockerRuntime::Colima;
        }
        if host.contains("docker.raw.sock") || host.contains("docker-desktop") {
            return DockerRuntime::DockerDesktop;
        }
    }

    // Check DOCKER_CONTEXT for runtime hints
    if let Ok(ctx) = std::env::var("DOCKER_CONTEXT") {
        let ctx_lower = ctx.to_lowercase();
        if ctx_lower.contains("orbstack") {
            return DockerRuntime::OrbStack;
        }
        if ctx_lower.contains("colima") {
            return DockerRuntime::Colima;
        }
        if ctx_lower.contains("desktop") {
            return DockerRuntime::DockerDesktop;
        }
    }

    match std::env::consts::OS {
        "macos" => {
            // Check for OrbStack installation
            if let Ok(home) = std::env::var("HOME")
                && Path::new(&format!("{home}/.orbstack")).exists()
            {
                return DockerRuntime::OrbStack;
            }
            // Default to Docker Desktop on macOS
            DockerRuntime::DockerDesktop
        }
        "linux" => DockerRuntime::LinuxNative,
        _ => DockerRuntime::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_some_variant() {
        // Just verify it doesn't panic
        let _runtime = detect_runtime();
    }

    #[test]
    fn runtime_variants_are_distinguishable() {
        assert_ne!(DockerRuntime::DockerDesktop, DockerRuntime::OrbStack);
        assert_ne!(DockerRuntime::LinuxNative, DockerRuntime::Unknown);
    }
}
