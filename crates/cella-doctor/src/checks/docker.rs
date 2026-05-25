//! Docker daemon and tooling checks.

use cella_backend::BackendKind;

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Run Docker-related diagnostics.
///
/// # Panics
///
/// Panics if `ctx.backend_kind` is `None` after matching `Some(_)`.
/// This is unreachable in practice.
pub async fn check_docker(ctx: &CheckContext) -> CategoryReport {
    if matches!(
        ctx.backend_kind,
        Some(kind) if !matches!(kind, BackendKind::Docker | BackendKind::Podman)
    ) {
        let backend = ctx.backend_kind.expect("matched Some(kind) guard");
        return CategoryReport::new(
            "Docker",
            vec![CheckResult {
                name: "skipped".into(),
                severity: Severity::Info,
                detail: format!("Docker checks skipped: selected backend is {backend}"),
                fix_hint: None,
            }],
        );
    }

    let mut checks = Vec::new();

    checks.push(check_daemon_reachable(ctx).await);

    if let Some(check) = check_socket_accessible() {
        checks.push(check);
    }

    checks.push(check_docker_cli().await);
    checks.push(check_engine_version().await);
    checks.push(check_engine_security_status().await);
    checks.push(check_buildx().await);
    checks.push(check_compose().await);

    CategoryReport::new("Docker", checks)
}

/// Check whether the Docker daemon is reachable via ping.
async fn check_daemon_reachable(ctx: &CheckContext) -> CheckResult {
    match ctx.backend_client {
        Some(ref client) => match client.ping().await {
            Ok(()) => CheckResult {
                name: "daemon reachable".into(),
                severity: Severity::Pass,
                detail: "Docker daemon is running".into(),
                fix_hint: None,
            },
            Err(e) => CheckResult {
                name: "daemon reachable".into(),
                severity: Severity::Error,
                detail: format!("ping failed: {e}"),
                fix_hint: Some("Is Docker running? Check `docker ps`".into()),
            },
        },
        None => CheckResult {
            name: "daemon reachable".into(),
            severity: Severity::Error,
            detail: "could not connect to Docker".into(),
            fix_hint: Some("Is Docker running? Check `docker ps`".into()),
        },
    }
}

/// Check that the Docker CLI is available in PATH.
async fn check_docker_cli() -> CheckResult {
    match tokio::process::Command::new("docker")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            CheckResult {
                name: "docker CLI".into(),
                severity: Severity::Pass,
                detail: version,
                fix_hint: None,
            }
        }
        _ => CheckResult {
            name: "docker CLI".into(),
            severity: Severity::Warning,
            detail: "not found in PATH".into(),
            fix_hint: Some("Ensure docker CLI is in your PATH".into()),
        },
    }
}

/// Check that Docker Buildx is available.
async fn check_buildx() -> CheckResult {
    match tokio::process::Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            CheckResult {
                name: "buildx".into(),
                severity: Severity::Pass,
                detail: version,
                fix_hint: None,
            }
        }
        _ => CheckResult {
            name: "buildx".into(),
            severity: Severity::Warning,
            detail: "not available".into(),
            fix_hint: Some("Install buildx for faster builds".into()),
        },
    }
}

/// Check that Docker Compose V2 is available.
async fn check_compose() -> CheckResult {
    match tokio::process::Command::new("docker")
        .args(["compose", "version"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            CheckResult {
                name: "compose".into(),
                severity: Severity::Pass,
                detail: version,
                fix_hint: None,
            }
        }
        _ => CheckResult {
            name: "compose".into(),
            severity: Severity::Warning,
            detail: "Docker Compose V2 not found".into(),
            fix_hint: Some(
                "Install Docker Compose V2: https://docs.docker.com/compose/install/".into(),
            ),
        },
    }
}

/// Report the Docker Engine version.
async fn check_engine_version() -> CheckResult {
    let output = tokio::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .await;

    let Ok(output) = output else {
        return CheckResult {
            name: "engine version".into(),
            severity: Severity::Warning,
            detail: "could not query Docker Engine version".into(),
            fix_hint: Some("Is Docker running?".into()),
        };
    };

    if !output.status.success() {
        return CheckResult {
            name: "engine version".into(),
            severity: Severity::Warning,
            detail: "docker version command failed".into(),
            fix_hint: None,
        };
    }

    let version_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    CheckResult {
        name: "engine version".into(),
        severity: Severity::Pass,
        detail: format!("Docker Engine {version_str}"),
        fix_hint: None,
    }
}

/// Query Docker Engine version and check for security-relevant thresholds.
async fn check_engine_security_status() -> CheckResult {
    let output = tokio::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .await;

    let Ok(output) = output else {
        return CheckResult {
            name: "engine security".into(),
            severity: Severity::Warning,
            detail: "could not query Docker Engine version for security check".into(),
            fix_hint: Some("Is Docker running?".into()),
        };
    };

    if !output.status.success() {
        return CheckResult {
            name: "engine security".into(),
            severity: Severity::Warning,
            detail: "docker version command failed".into(),
            fix_hint: None,
        };
    }

    let version_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    check_engine_security(&version_str)
}

/// Parse a version string and check against known security thresholds.
///
/// Handles both bare versions (`"29.3.1"`) and verbose Docker output
/// (`"Docker version 29.3.1, build abc123"`).
///
/// # Security thresholds
///
/// - `< 29.0`: default seccomp profile does not block `AF_ALG`
///   (CVE-2026-31431 mitigation). Credential protection benefits from
///   29+ hardening.
/// - `29.0 – 29.3.0`: CVE-2026-34040 — `AuthZ` bypass via oversized
///   requests. Upgrade to 29.3.1+.
/// - `>= 29.3.1`: all known security-relevant patches applied.
fn check_engine_security(version_str: &str) -> CheckResult {
    let Some((major, minor, patch)) = parse_version(version_str) else {
        return CheckResult {
            name: "engine security".into(),
            severity: Severity::Info,
            detail: format!("could not parse version \"{version_str}\" for security check"),
            fix_hint: None,
        };
    };

    if major >= 29 && (major > 29 || minor > 3 || (minor == 3 && patch >= 1)) {
        return CheckResult {
            name: "engine security".into(),
            severity: Severity::Pass,
            detail: "all known security-relevant patches applied".into(),
            fix_hint: None,
        };
    }

    if major >= 29 {
        return CheckResult {
            name: "engine security".into(),
            severity: Severity::Warning,
            detail: format!(
                "Docker Engine {major}.{minor}.{patch} — \
                 CVE-2026-34040: AuthZ bypass via oversized requests. \
                 Upgrade to 29.3.1+"
            ),
            fix_hint: Some("Update Docker Engine to 29.3.1+".into()),
        };
    }

    CheckResult {
        name: "engine security".into(),
        severity: Severity::Warning,
        detail: format!(
            "Docker Engine {major}.{minor}.{patch} — \
             default seccomp profile does not block AF_ALG \
             (CVE-2026-31431 mitigation). \
             Credential protection benefits from 29+ hardening"
        ),
        fix_hint: Some("Update Docker Engine to 29+".into()),
    }
}

/// Extract `(major, minor, patch)` from a version string.
///
/// Handles bare versions like `"29.3.1"` and verbose Docker output
/// like `"Docker version 29.3.1, build abc123"`.
fn parse_version(input: &str) -> Option<(u32, u32, u32)> {
    // Find the first substring that looks like a semver triple.
    // Walk through words looking for one containing digits and dots.
    for word in input.split(|c: char| c.is_ascii_whitespace() || c == ',') {
        let parts: Vec<&str> = word.split('.').collect();
        if parts.len() >= 2 {
            let nums: Vec<u32> = parts.iter().filter_map(|s| s.parse().ok()).collect();
            match nums.as_slice() {
                [maj, min, pat, ..] => return Some((*maj, *min, *pat)),
                [maj, min] => return Some((*maj, *min, 0)),
                _ => {}
            }
        }
    }
    None
}

/// Check Docker socket accessibility, including alternative runtime paths.
fn check_socket_accessible() -> Option<CheckResult> {
    // If DOCKER_HOST is TCP, socket check is not applicable
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if host.starts_with("tcp://") || host.starts_with("http://") || host.starts_with("https://")
        {
            return None;
        }
        // Extract unix socket path
        if let Some(path) = host.strip_prefix("unix://") {
            return Some(check_path_accessible(path));
        }
    }

    // Default socket path
    if std::fs::metadata("/var/run/docker.sock").is_ok() {
        return Some(check_path_accessible("/var/run/docker.sock"));
    }

    // Try alternative runtime discovery
    if let Some(discovered) = cella_docker::discovery::discover_socket() {
        return Some(CheckResult {
            name: "socket accessible".into(),
            severity: Severity::Pass,
            detail: format!(
                "{} (discovered via {})",
                discovered.path.display(),
                discovered.method,
            ),
            fix_hint: None,
        });
    }

    // No socket found anywhere
    Some(CheckResult {
        name: "socket accessible".into(),
        severity: Severity::Error,
        detail: "no Docker socket found".into(),
        fix_hint: Some(
            "Set DOCKER_HOST or ensure your container runtime is running \
             (Docker Desktop, Colima, Podman, Rancher Desktop)"
                .into(),
        ),
    })
}

fn check_path_accessible(path: &str) -> CheckResult {
    match std::fs::metadata(path) {
        Ok(_) => CheckResult {
            name: "socket accessible".into(),
            severity: Severity::Pass,
            detail: path.to_string(),
            fix_hint: None,
        },
        Err(e) => CheckResult {
            name: "socket accessible".into(),
            severity: Severity::Error,
            detail: format!("{path}: {e}"),
            fix_hint: Some(format!("Check permissions: `ls -la {path}`")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_path_accessible_existing_path() {
        // /tmp always exists on Linux
        let result = check_path_accessible("/tmp");
        assert_eq!(result.severity, Severity::Pass);
        assert_eq!(result.detail, "/tmp");
        assert_eq!(result.name, "socket accessible");
    }

    #[test]
    fn check_path_accessible_nonexistent_path() {
        let result = check_path_accessible("/nonexistent/path/that/does/not/exist.sock");
        assert_eq!(result.severity, Severity::Error);
        assert!(result.detail.contains("/nonexistent/path"));
        assert!(result.fix_hint.is_some());
    }

    #[test]
    fn check_path_accessible_with_tempfile() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let result = check_path_accessible(path);
        assert_eq!(result.severity, Severity::Pass);
        assert_eq!(result.detail, path);
    }

    #[tokio::test]
    async fn check_docker_no_client_returns_error() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            backend_kind: None,
            backend_client: None,
        };
        let report = check_docker(&ctx).await;
        assert_eq!(report.name, "Docker");

        let daemon_check = report
            .checks
            .iter()
            .find(|c| c.name == "daemon reachable")
            .expect("should have daemon reachable check");
        assert_eq!(daemon_check.severity, Severity::Error);
        assert!(daemon_check.detail.contains("could not connect"));
    }

    #[tokio::test]
    async fn check_docker_no_client_has_fix_hint() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            backend_kind: None,
            backend_client: None,
        };
        let report = check_docker(&ctx).await;
        let daemon_check = report
            .checks
            .iter()
            .find(|c| c.name == "daemon reachable")
            .unwrap();
        assert!(daemon_check.fix_hint.is_some());
        assert!(
            daemon_check
                .fix_hint
                .as_ref()
                .unwrap()
                .contains("Docker running")
        );
    }

    #[tokio::test]
    async fn check_docker_no_client_has_cli_and_buildx_and_compose_checks() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            backend_kind: None,
            backend_client: None,
        };
        let report = check_docker(&ctx).await;
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"daemon reachable"),
            "should have daemon reachable"
        );
        assert!(names.contains(&"docker CLI"), "should have docker CLI");
        assert!(names.contains(&"buildx"), "should have buildx");
        assert!(names.contains(&"compose"), "should have compose");
    }

    #[test]
    fn check_path_accessible_error_has_fix_hint_with_path() {
        let path = "/nonexistent/socket.sock";
        let result = check_path_accessible(path);
        assert_eq!(result.severity, Severity::Error);
        let hint = result.fix_hint.unwrap();
        assert!(
            hint.contains(path),
            "fix_hint should contain the path, got: {hint}"
        );
    }

    #[test]
    fn check_path_accessible_pass_has_no_fix_hint() {
        let result = check_path_accessible("/tmp");
        assert!(result.fix_hint.is_none());
    }

    #[tokio::test]
    async fn check_daemon_reachable_no_client() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            backend_kind: None,
            backend_client: None,
        };
        let result = check_daemon_reachable(&ctx).await;
        assert_eq!(result.name, "daemon reachable");
        assert_eq!(result.severity, Severity::Error);
        assert_eq!(result.detail, "could not connect to Docker");
    }

    #[tokio::test]
    async fn check_docker_report_has_at_least_five_checks() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            backend_kind: None,
            backend_client: None,
        };
        let report = check_docker(&ctx).await;
        // At minimum: daemon reachable + docker CLI + engine version +
        //             engine security + buildx + compose
        // (socket check may or may not be present depending on environment)
        assert!(
            report.checks.len() >= 5,
            "expected at least 5 checks, got {}",
            report.checks.len()
        );
    }

    // --- Engine security check tests ---

    #[test]
    fn engine_security_old_version_warns_seccomp() {
        let result = check_engine_security("28.0.0");
        assert_eq!(result.name, "engine security");
        assert_eq!(result.severity, Severity::Warning);
        assert!(
            result.detail.contains("seccomp"),
            "detail: {}",
            result.detail
        );
        assert!(
            result.detail.contains("AF_ALG"),
            "detail: {}",
            result.detail
        );
        assert!(result.fix_hint.is_some());
    }

    #[test]
    fn engine_security_29_0_warns_authz_bypass() {
        let result = check_engine_security("29.0.0");
        assert_eq!(result.severity, Severity::Warning);
        assert!(
            result.detail.contains("CVE-2026-34040"),
            "detail: {}",
            result.detail
        );
        assert!(result.fix_hint.is_some());
    }

    #[test]
    fn engine_security_29_3_0_warns_authz_bypass() {
        let result = check_engine_security("29.3.0");
        assert_eq!(result.severity, Severity::Warning);
        assert!(
            result.detail.contains("CVE-2026-34040"),
            "detail: {}",
            result.detail
        );
    }

    #[test]
    fn engine_security_29_3_1_passes() {
        let result = check_engine_security("29.3.1");
        assert_eq!(result.severity, Severity::Pass);
        assert!(
            result
                .detail
                .contains("all known security-relevant patches"),
            "detail: {}",
            result.detail
        );
        assert!(result.fix_hint.is_none());
    }

    #[test]
    fn engine_security_30_0_0_passes() {
        let result = check_engine_security("30.0.0");
        assert_eq!(result.severity, Severity::Pass);
    }

    #[test]
    fn engine_security_malformed_returns_info() {
        let result = check_engine_security("not-a-version");
        assert_eq!(result.severity, Severity::Info);
        assert!(
            result.detail.contains("could not parse"),
            "detail: {}",
            result.detail
        );
    }

    #[test]
    fn engine_security_verbose_docker_output() {
        let result = check_engine_security("Docker version 29.3.1, build abc123");
        assert_eq!(result.severity, Severity::Pass);
    }

    #[test]
    fn engine_security_verbose_old_version() {
        let result = check_engine_security("Docker version 28.1.2, build def456");
        assert_eq!(result.severity, Severity::Warning);
        assert!(result.detail.contains("seccomp"));
    }

    // --- parse_version tests ---

    #[test]
    fn parse_version_bare() {
        assert_eq!(parse_version("29.3.1"), Some((29, 3, 1)));
    }

    #[test]
    fn parse_version_two_parts() {
        assert_eq!(parse_version("29.3"), Some((29, 3, 0)));
    }

    #[test]
    fn parse_version_verbose() {
        assert_eq!(
            parse_version("Docker version 29.3.1, build abc123"),
            Some((29, 3, 1))
        );
    }

    #[test]
    fn parse_version_garbage() {
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[test]
    fn parse_version_empty() {
        assert_eq!(parse_version(""), None);
    }
}
