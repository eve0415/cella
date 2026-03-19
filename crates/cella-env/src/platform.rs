//! Docker runtime detection for platform-specific SSH agent forwarding.

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
        if ctx_lower.contains("desktop") {
            return DockerRuntime::DockerDesktop;
        }
    }

    // 3. Query the active Docker context endpoint for reliable detection
    if let Some(runtime) = detect_from_docker_context() {
        return runtime;
    }

    // 4. Platform-based fallback
    match std::env::consts::OS {
        "linux" => DockerRuntime::LinuxNative,
        _ => DockerRuntime::Unknown,
    }
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
}
