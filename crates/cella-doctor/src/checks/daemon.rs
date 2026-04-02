//! Cella daemon health checks.

use cella_protocol::{ManagementRequest, ManagementResponse};

use super::{CategoryReport, CheckResult, Severity};

/// Run daemon diagnostics.
pub async fn check_daemon() -> CategoryReport {
    let mut checks = Vec::new();

    let Some(data_dir) = cella_env::paths::cella_data_dir() else {
        checks.push(CheckResult {
            name: "data directory".into(),
            severity: Severity::Warning,
            detail: "could not determine ~/.cella directory".into(),
            fix_hint: Some("Ensure $HOME is set".into()),
        });
        return CategoryReport::new("Daemon", checks);
    };

    let pid_path = data_dir.join("daemon.pid");
    let socket_path = data_dir.join("daemon.sock");

    // Daemon running
    let running = cella_daemon::daemon::is_daemon_running(&pid_path, &socket_path);
    if running {
        checks.push(CheckResult {
            name: "running".into(),
            severity: Severity::Pass,
            detail: "daemon is running".into(),
            fix_hint: None,
        });
    } else {
        checks.push(CheckResult {
            name: "running".into(),
            severity: Severity::Warning,
            detail: "daemon is not running".into(),
            fix_hint: Some("Starts automatically with `cella up`".into()),
        });
        return CategoryReport::new("Daemon", checks);
    }

    // Daemon version match
    match cella_daemon::management::send_management_request(
        &socket_path,
        &ManagementRequest::QueryStatus,
    )
    .await
    {
        Ok(ManagementResponse::Status { daemon_version, .. }) => {
            let cli_version = env!("CARGO_PKG_VERSION");
            if daemon_version.is_empty() {
                checks.push(CheckResult {
                    name: "version".into(),
                    severity: Severity::Warning,
                    detail: "old daemon without version support".into(),
                    fix_hint: Some("Restart: `cella daemon stop && cella daemon start`".into()),
                });
            } else if daemon_version == cli_version {
                checks.push(CheckResult {
                    name: "version".into(),
                    severity: Severity::Pass,
                    detail: daemon_version,
                    fix_hint: None,
                });
            } else {
                checks.push(CheckResult {
                    name: "version".into(),
                    severity: Severity::Warning,
                    detail: format!("daemon {daemon_version} != CLI {cli_version}"),
                    fix_hint: Some("Restart: `cella daemon stop && cella daemon start`".into()),
                });
            }
        }
        Ok(_) => {
            checks.push(CheckResult {
                name: "version".into(),
                severity: Severity::Warning,
                detail: "unexpected response from daemon".into(),
                fix_hint: Some("Restart: `cella daemon stop && cella daemon start`".into()),
            });
        }
        Err(e) => {
            checks.push(CheckResult {
                name: "version".into(),
                severity: Severity::Warning,
                detail: format!("could not query daemon: {e}"),
                fix_hint: None,
            });
        }
    }

    CategoryReport::new("Daemon", checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_daemon_returns_daemon_category() {
        let report = check_daemon().await;
        assert_eq!(report.name, "Daemon");
        // Should always have at least one check result
        assert!(!report.checks.is_empty());
    }

    #[tokio::test]
    async fn check_daemon_first_check_is_data_dir_or_running() {
        let report = check_daemon().await;
        let first = &report.checks[0];
        // Either "data directory" (if HOME is unset/cella dir missing) or "running"
        assert!(
            first.name == "data directory" || first.name == "running",
            "unexpected first check name: '{}'",
            first.name
        );
    }

    #[tokio::test]
    async fn check_daemon_has_at_least_one_check() {
        let report = check_daemon().await;
        assert!(
            !report.checks.is_empty(),
            "daemon report should have at least one check"
        );
    }

    #[tokio::test]
    async fn check_daemon_running_check_severity() {
        let report = check_daemon().await;
        let running = report.checks.iter().find(|c| c.name == "running");
        if let Some(check) = running {
            // Running check is either Pass (running) or Warning (not running)
            assert!(
                check.severity == Severity::Pass || check.severity == Severity::Warning,
                "running check severity should be Pass or Warning, got {:?}",
                check.severity
            );
        }
    }

    #[tokio::test]
    async fn check_daemon_not_running_has_fix_hint() {
        let report = check_daemon().await;
        let running = report.checks.iter().find(|c| c.name == "running");
        if let Some(check) = running
            && check.severity == Severity::Warning
        {
            assert!(
                check.fix_hint.is_some(),
                "not-running check should have a fix_hint"
            );
            assert!(
                check.fix_hint.as_ref().unwrap().contains("cella up"),
                "fix_hint should mention 'cella up'"
            );
        }
    }

    #[tokio::test]
    async fn check_daemon_data_dir_warning_has_fix_hint() {
        let report = check_daemon().await;
        let data_dir = report.checks.iter().find(|c| c.name == "data directory");
        if let Some(check) = data_dir {
            assert_eq!(check.severity, Severity::Warning);
            assert!(check.fix_hint.is_some());
            assert!(
                check.fix_hint.as_ref().unwrap().contains("HOME"),
                "fix_hint should mention HOME"
            );
        }
    }
}
