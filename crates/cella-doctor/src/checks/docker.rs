//! Docker daemon and tooling checks.

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Run Docker-related diagnostics.
pub async fn check_docker(ctx: &CheckContext) -> CategoryReport {
    let mut checks = Vec::new();

    // Docker daemon reachable
    match ctx.docker_client {
        Some(ref client) => match client.ping().await {
            Ok(()) => {
                checks.push(CheckResult {
                    name: "daemon reachable".into(),
                    severity: Severity::Pass,
                    detail: "Docker daemon is running".into(),
                    fix_hint: None,
                });
            }
            Err(e) => {
                checks.push(CheckResult {
                    name: "daemon reachable".into(),
                    severity: Severity::Error,
                    detail: format!("ping failed: {e}"),
                    fix_hint: Some("Is Docker running? Check `docker ps`".into()),
                });
            }
        },
        None => {
            checks.push(CheckResult {
                name: "daemon reachable".into(),
                severity: Severity::Error,
                detail: "could not connect to Docker".into(),
                fix_hint: Some("Is Docker running? Check `docker ps`".into()),
            });
        }
    }

    // Docker socket accessible
    if let Some(check) = check_socket_accessible() {
        checks.push(check);
    }

    // Docker CLI available
    match tokio::process::Command::new("docker")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            checks.push(CheckResult {
                name: "docker CLI".into(),
                severity: Severity::Pass,
                detail: version,
                fix_hint: None,
            });
        }
        _ => {
            checks.push(CheckResult {
                name: "docker CLI".into(),
                severity: Severity::Warning,
                detail: "not found in PATH".into(),
                fix_hint: Some("Ensure docker CLI is in your PATH".into()),
            });
        }
    }

    // buildx available
    match tokio::process::Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            checks.push(CheckResult {
                name: "buildx".into(),
                severity: Severity::Pass,
                detail: version,
                fix_hint: None,
            });
        }
        _ => {
            checks.push(CheckResult {
                name: "buildx".into(),
                severity: Severity::Warning,
                detail: "not available".into(),
                fix_hint: Some("Install buildx for faster builds".into()),
            });
        }
    }

    CategoryReport::new("Docker", checks)
}

/// Check Docker socket accessibility (Linux/macOS only, skip for TCP hosts).
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
    Some(check_path_accessible("/var/run/docker.sock"))
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
