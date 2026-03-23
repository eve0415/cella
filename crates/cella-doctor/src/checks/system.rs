//! System information checks (always shown, informational only).

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Collect system information.
pub async fn check_system(ctx: &CheckContext) -> CategoryReport {
    let mut checks = Vec::new();

    // cella version
    checks.push(CheckResult {
        name: "cella".into(),
        severity: Severity::Info,
        detail: env!("CARGO_PKG_VERSION").into(),
        fix_hint: None,
    });

    // Platform
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let distro = read_os_release().unwrap_or_default();
    let platform = if distro.is_empty() {
        format!("{os} ({arch})")
    } else {
        format!("{distro} ({arch})")
    };
    checks.push(CheckResult {
        name: "platform".into(),
        severity: Severity::Info,
        detail: platform,
        fix_hint: None,
    });

    // Docker runtime
    let runtime = cella_env::platform::detect_runtime();
    checks.push(CheckResult {
        name: "docker runtime".into(),
        severity: Severity::Info,
        detail: format!("{runtime}"),
        fix_hint: None,
    });

    // Docker version
    let docker_version = if let Some(ref client) = ctx.docker_client {
        match client.inner().version().await {
            Ok(ver) => ver.version.unwrap_or_else(|| "unknown".into()),
            Err(_) => docker_cli_version().await,
        }
    } else {
        docker_cli_version().await
    };
    checks.push(CheckResult {
        name: "docker version".into(),
        severity: Severity::Info,
        detail: docker_version,
        fix_hint: None,
    });

    // Shell
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".into());
    checks.push(CheckResult {
        name: "shell".into(),
        severity: Severity::Info,
        detail: shell,
        fix_hint: None,
    });

    CategoryReport::new("System Info", checks)
}

/// Read distro name from /etc/os-release (Linux only).
fn read_os_release() -> Option<String> {
    let content = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

/// Fallback: get Docker version from CLI.
async fn docker_cli_version() -> String {
    match tokio::process::Command::new("docker")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "unavailable".into(),
    }
}
