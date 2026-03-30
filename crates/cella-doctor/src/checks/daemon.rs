//! Cella daemon health checks.

use cella_port::protocol::{ManagementRequest, ManagementResponse};

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
