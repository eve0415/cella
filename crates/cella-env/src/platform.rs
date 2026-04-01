//! Docker runtime detection for platform-specific SSH agent forwarding.

/// Detected Docker runtime environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerRuntime {
    /// Docker Desktop (macOS/Windows) — provides SSH agent via VM.
    DockerDesktop,
    /// `OrbStack` (macOS) — uses `/run/host-services/ssh-auth.sock` for SSH agent
    /// (like Docker Desktop).
    OrbStack,
    /// Native Docker on Linux — direct socket bind-mount works.
    LinuxNative,
    /// Colima (macOS) — direct socket bind-mount works.
    Colima,
    /// Podman — VM-based on macOS (Podman Machine), native on Linux.
    Podman,
    /// Rancher Desktop — VM-based (Lima) on macOS/Linux.
    RancherDesktop,
    /// Unknown runtime — try direct bind-mount with warning.
    Unknown,
}

impl DockerRuntime {
    /// Stable label value for container metadata.
    /// These strings are persisted in Docker labels — do not rename.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::DockerDesktop => "docker-desktop",
            Self::OrbStack => "orbstack",
            Self::LinuxNative => "linux-native",
            Self::Colima => "colima",
            Self::Podman => "podman",
            Self::RancherDesktop => "rancher-desktop",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for DockerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_label())
    }
}

/// Detect the Docker runtime from environment variables and Docker context.
pub fn detect_runtime() -> DockerRuntime {
    // 1. Check DOCKER_HOST for runtime-specific patterns (fast, reliable when set)
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if host.contains("orbstack") {
            return DockerRuntime::OrbStack;
        }
        if host.contains("colima") {
            return DockerRuntime::Colima;
        }
        if host.contains("podman") {
            return DockerRuntime::Podman;
        }
        if host.contains(".rd/") || host.contains("rancher") {
            return DockerRuntime::RancherDesktop;
        }
        if host.contains("docker.raw.sock") || host.contains("docker-desktop") {
            return DockerRuntime::DockerDesktop;
        }
    }

    // 2. Check DOCKER_CONTEXT for runtime hints (fast, reliable when set)
    if let Ok(ctx) = std::env::var("DOCKER_CONTEXT") {
        let ctx_lower = ctx.to_lowercase();
        if ctx_lower.contains("orbstack") {
            return DockerRuntime::OrbStack;
        }
        if ctx_lower.contains("colima") {
            return DockerRuntime::Colima;
        }
        if ctx_lower.contains("podman") {
            return DockerRuntime::Podman;
        }
        if ctx_lower.contains("rancher") {
            return DockerRuntime::RancherDesktop;
        }
        if ctx_lower.contains("desktop") {
            return DockerRuntime::DockerDesktop;
        }
    }

    // 3. Query the active Docker context endpoint for reliable detection
    if let Some(runtime) = detect_from_docker_context() {
        return runtime;
    }

    // 4. Platform-based fallback
    if std::env::consts::OS == "linux" {
        // Check for rootless Podman socket before assuming native Docker
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
            && std::path::Path::new(&format!("{xdg}/podman/podman.sock")).exists()
        {
            return DockerRuntime::Podman;
        }
        return DockerRuntime::LinuxNative;
    }

    DockerRuntime::Unknown
}

/// Query `docker context inspect` to determine the active runtime.
fn detect_from_docker_context() -> Option<DockerRuntime> {
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
        return None;
    }

    let endpoint = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_lowercase();

    if endpoint.contains("orbstack") {
        return Some(DockerRuntime::OrbStack);
    }
    if endpoint.contains("colima") {
        return Some(DockerRuntime::Colima);
    }
    if endpoint.contains("podman") {
        return Some(DockerRuntime::Podman);
    }
    if endpoint.contains(".rd/") || endpoint.contains("rancher") {
        return Some(DockerRuntime::RancherDesktop);
    }
    if endpoint.contains("docker.raw.sock") || endpoint.contains("docker-desktop") {
        return Some(DockerRuntime::DockerDesktop);
    }

    None
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

    #[test]
    fn as_label_returns_stable_strings() {
        assert_eq!(DockerRuntime::DockerDesktop.as_label(), "docker-desktop");
        assert_eq!(DockerRuntime::OrbStack.as_label(), "orbstack");
        assert_eq!(DockerRuntime::LinuxNative.as_label(), "linux-native");
        assert_eq!(DockerRuntime::Colima.as_label(), "colima");
        assert_eq!(DockerRuntime::Podman.as_label(), "podman");
        assert_eq!(DockerRuntime::RancherDesktop.as_label(), "rancher-desktop");
        assert_eq!(DockerRuntime::Unknown.as_label(), "unknown");
    }

    #[test]
    fn display_matches_as_label() {
        for runtime in [
            DockerRuntime::DockerDesktop,
            DockerRuntime::OrbStack,
            DockerRuntime::LinuxNative,
            DockerRuntime::Colima,
            DockerRuntime::Podman,
            DockerRuntime::RancherDesktop,
            DockerRuntime::Unknown,
        ] {
            assert_eq!(format!("{runtime}"), runtime.as_label());
        }
    }

    #[test]
    fn runtime_clone_and_debug() {
        let runtime = DockerRuntime::OrbStack;
        let cloned = runtime.clone();
        assert_eq!(runtime, cloned);
        let debug = format!("{runtime:?}");
        assert!(debug.contains("OrbStack"));
    }

    #[test]
    fn all_variants_have_unique_labels() {
        let variants = [
            DockerRuntime::DockerDesktop,
            DockerRuntime::OrbStack,
            DockerRuntime::LinuxNative,
            DockerRuntime::Colima,
            DockerRuntime::Podman,
            DockerRuntime::RancherDesktop,
            DockerRuntime::Unknown,
        ];
        let labels: Vec<&str> = variants.iter().map(DockerRuntime::as_label).collect();
        for (i, a) in labels.iter().enumerate() {
            for b in labels.iter().skip(i + 1) {
                assert_ne!(a, b, "duplicate label found: {a}");
            }
        }
    }

    #[test]
    fn detect_runtime_does_not_panic() {
        // Integration-style: detect_runtime reads real env + may call docker.
        // Just verify it returns a valid variant without panicking.
        let runtime = detect_runtime();
        // Label should be a non-empty string
        assert!(!runtime.as_label().is_empty());
    }
}
