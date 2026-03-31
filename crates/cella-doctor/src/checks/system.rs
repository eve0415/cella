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

/// Read distro name from a custom path (test helper).
#[cfg(test)]
fn read_os_release_from(content: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_no_docker() -> CheckContext {
        CheckContext {
            workspace_folder: None,
            all: false,
            docker_client: None,
        }
    }

    #[tokio::test]
    async fn check_system_returns_expected_categories() {
        let ctx = ctx_no_docker();
        let report = check_system(&ctx).await;
        assert_eq!(report.name, "System Info");

        // Must contain at least: cella, platform, docker runtime, docker version, shell
        assert!(report.checks.len() >= 5);

        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"cella"));
        assert!(names.contains(&"platform"));
        assert!(names.contains(&"docker runtime"));
        assert!(names.contains(&"docker version"));
        assert!(names.contains(&"shell"));
    }

    #[tokio::test]
    async fn check_system_all_checks_are_info() {
        let ctx = ctx_no_docker();
        let report = check_system(&ctx).await;
        for check in &report.checks {
            assert_eq!(
                check.severity,
                Severity::Info,
                "check '{}' should be Info, got {:?}",
                check.name,
                check.severity
            );
        }
    }

    #[tokio::test]
    async fn check_system_cella_version_matches_cargo_pkg() {
        let ctx = ctx_no_docker();
        let report = check_system(&ctx).await;
        let cella_check = report
            .checks
            .iter()
            .find(|c| c.name == "cella")
            .expect("should have cella check");
        assert_eq!(cella_check.detail, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn check_system_platform_contains_arch() {
        let ctx = ctx_no_docker();
        let report = check_system(&ctx).await;
        let platform = report
            .checks
            .iter()
            .find(|c| c.name == "platform")
            .expect("should have platform check");
        assert!(
            platform.detail.contains(std::env::consts::ARCH),
            "platform detail '{}' should contain arch '{}'",
            platform.detail,
            std::env::consts::ARCH
        );
    }

    #[test]
    fn read_os_release_from_parses_pretty_name() {
        let content = r#"NAME="Ubuntu"
VERSION="22.04.3 LTS (Jammy Jellyfish)"
PRETTY_NAME="Ubuntu 22.04.3 LTS"
ID=ubuntu"#;
        assert_eq!(
            read_os_release_from(content),
            Some("Ubuntu 22.04.3 LTS".to_string())
        );
    }

    #[test]
    fn read_os_release_from_returns_none_without_pretty_name() {
        let content = "NAME=\"Alpine Linux\"\nID=alpine";
        assert_eq!(read_os_release_from(content), None);
    }

    #[test]
    fn read_os_release_from_empty_content() {
        assert_eq!(read_os_release_from(""), None);
    }
}
