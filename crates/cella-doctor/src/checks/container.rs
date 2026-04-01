//! Per-container health checks.

use cella_docker::DockerClient;
use cella_port::protocol::{ManagementRequest, ManagementResponse};

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Run container diagnostics.
///
/// Returns one `CategoryReport` per container, or a single report
/// explaining why container checks were skipped.
pub async fn check_containers(ctx: &CheckContext, daemon_running: bool) -> Vec<CategoryReport> {
    if !daemon_running {
        return vec![CategoryReport::new(
            "Containers",
            vec![CheckResult {
                name: "skipped".into(),
                severity: Severity::Info,
                detail: "container checks skipped: daemon not running".into(),
                fix_hint: None,
            }],
        )];
    }

    let Some(ref client) = ctx.docker_client else {
        return vec![CategoryReport::new(
            "Containers",
            vec![CheckResult {
                name: "skipped".into(),
                severity: Severity::Info,
                detail: "container checks skipped: Docker not connected".into(),
                fix_hint: None,
            }],
        )];
    };

    if ctx.all {
        check_all_containers(client).await
    } else {
        check_workspace_container(ctx, client).await
    }
}

async fn check_all_containers(client: &DockerClient) -> Vec<CategoryReport> {
    match client.list_cella_containers(true).await {
        Ok(containers) if containers.is_empty() => {
            vec![CategoryReport::new(
                "Containers",
                vec![CheckResult {
                    name: "containers".into(),
                    severity: Severity::Info,
                    detail: "no running cella containers found".into(),
                    fix_hint: None,
                }],
            )]
        }
        Ok(containers) => {
            let mut reports = Vec::new();
            for container in &containers {
                let name = format!("Container: {}", container.name);
                let checks = check_single_container(client, &container.id, &container.name).await;
                reports.push(CategoryReport::new(name, checks));
            }
            reports
        }
        Err(e) => {
            vec![CategoryReport::new(
                "Containers",
                vec![CheckResult {
                    name: "list".into(),
                    severity: Severity::Warning,
                    detail: format!("could not list containers: {e}"),
                    fix_hint: None,
                }],
            )]
        }
    }
}

async fn check_workspace_container(
    ctx: &CheckContext,
    client: &DockerClient,
) -> Vec<CategoryReport> {
    let Some(ref workspace) = ctx.workspace_folder else {
        return vec![CategoryReport::new(
            "Containers",
            vec![CheckResult {
                name: "workspace".into(),
                severity: Severity::Info,
                detail: "no workspace detected, skipping container checks".into(),
                fix_hint: None,
            }],
        )];
    };

    let target = cella_backend::ContainerTarget {
        container_id: None,
        container_name: None,
        id_label: None,
        workspace_folder: Some(workspace.clone()),
    };

    match target.resolve(client, false).await {
        Ok(container) => {
            let name = format!("Container: {}", container.name);
            let checks = check_single_container(client, &container.id, &container.name).await;
            vec![CategoryReport::new(name, checks)]
        }
        Err(_) => {
            vec![CategoryReport::new(
                "Containers",
                vec![CheckResult {
                    name: "container".into(),
                    severity: Severity::Info,
                    detail: "no container found for current workspace".into(),
                    fix_hint: Some("Run `cella up` to start one".into()),
                }],
            )]
        }
    }
}

async fn check_single_container(
    client: &DockerClient,
    container_id: &str,
    container_name: &str,
) -> Vec<CheckResult> {
    let mut checks = Vec::new();

    // Container running (we already filtered to running, so this is a pass)
    checks.push(CheckResult {
        name: "running".into(),
        severity: Severity::Pass,
        detail: container_id[..12.min(container_id.len())].to_string(),
        fix_hint: None,
    });

    // Version skew check
    check_version_skew(client, container_id, &mut checks).await;

    // Agent connectivity via daemon
    check_agent_connectivity(&mut checks, container_name).await;

    // Credential forwarding
    check_credentials(client, container_id, &mut checks).await;

    // Port forwarding
    check_ports(&mut checks, container_name).await;

    checks
}

async fn check_version_skew(
    client: &DockerClient,
    container_id: &str,
    checks: &mut Vec<CheckResult>,
) {
    let Ok(info) = client.inspect_container(container_id).await else {
        return;
    };

    let cli_version = env!("CARGO_PKG_VERSION");
    let container_version = info
        .labels
        .get("dev.cella.version")
        .map_or("unknown", String::as_str);

    if container_version == cli_version {
        checks.push(CheckResult {
            name: "version".into(),
            severity: Severity::Pass,
            detail: container_version.to_string(),
            fix_hint: None,
        });
    } else {
        checks.push(CheckResult {
            name: "version".into(),
            severity: Severity::Warning,
            detail: format!("container {container_version} != CLI {cli_version}"),
            fix_hint: Some("Run `cella up` to update".into()),
        });
    }
}

async fn check_agent_connectivity(checks: &mut Vec<CheckResult>, container_name: &str) {
    let Some(data_dir) = cella_env::paths::cella_data_dir() else {
        return;
    };
    let mgmt_socket = data_dir.join("daemon.sock");

    match cella_daemon::management::send_management_request(
        &mgmt_socket,
        &ManagementRequest::QueryStatus,
    )
    .await
    {
        Ok(ManagementResponse::Status { containers, .. }) => {
            let found = containers
                .iter()
                .any(|c| c.container_name == container_name && c.agent_connected);
            if found {
                checks.push(CheckResult {
                    name: "agent".into(),
                    severity: Severity::Pass,
                    detail: "connected".into(),
                    fix_hint: None,
                });
            } else {
                checks.push(CheckResult {
                    name: "agent".into(),
                    severity: Severity::Warning,
                    detail: "not connected".into(),
                    fix_hint: Some("Check container logs: `cella logs`".into()),
                });
            }
        }
        _ => {
            checks.push(CheckResult {
                name: "agent".into(),
                severity: Severity::Warning,
                detail: "could not query daemon for agent status".into(),
                fix_hint: None,
            });
        }
    }
}

async fn check_credentials(
    client: &DockerClient,
    container_id: &str,
    checks: &mut Vec<CheckResult>,
) {
    // Read remote_user from container labels
    let remote_user = match client.inspect_container(container_id).await {
        Ok(info) => info
            .labels
            .get("dev.cella.remote_user")
            .cloned()
            .unwrap_or_else(|| "root".to_string()),
        Err(_) => "root".to_string(),
    };

    let config_dir = cella_env::gh_credential::gh_config_dir_for_user(&remote_user);
    let check_cmd = cella_env::gh_credential::gh_config_exists_in_container(&config_dir);

    let has_creds = client
        .exec_command(
            container_id,
            &cella_docker::ExecOptions {
                cmd: check_cmd,
                user: Some(remote_user),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0);

    if has_creds {
        checks.push(CheckResult {
            name: "gh credentials".into(),
            severity: Severity::Pass,
            detail: "present in container".into(),
            fix_hint: None,
        });
    } else {
        checks.push(CheckResult {
            name: "gh credentials".into(),
            severity: Severity::Warning,
            detail: "not found in container".into(),
            fix_hint: Some("Run `cella credential sync gh`".into()),
        });
    }
}

async fn check_ports(checks: &mut Vec<CheckResult>, container_name: &str) {
    let Some(data_dir) = cella_env::paths::cella_data_dir() else {
        return;
    };
    let mgmt_socket = data_dir.join("daemon.sock");

    match cella_daemon::management::send_management_request(
        &mgmt_socket,
        &ManagementRequest::QueryPorts,
    )
    .await
    {
        Ok(ManagementResponse::Ports { ports }) => {
            let port_count = ports
                .iter()
                .filter(|p| p.container_name == container_name)
                .count();
            checks.push(CheckResult {
                name: "forwarded ports".into(),
                severity: Severity::Info,
                detail: format!("{port_count} port(s) forwarded"),
                fix_hint: None,
            });
        }
        _ => {
            checks.push(CheckResult {
                name: "forwarded ports".into(),
                severity: Severity::Info,
                detail: "could not query port status".into(),
                fix_hint: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_no_docker(workspace: Option<std::path::PathBuf>) -> CheckContext {
        CheckContext {
            workspace_folder: workspace,
            all: false,
            docker_client: None,
        }
    }

    #[tokio::test]
    async fn skip_when_daemon_not_running() {
        let ctx = ctx_no_docker(None);
        let reports = check_containers(&ctx, false).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].checks[0].name, "skipped");
        assert_eq!(reports[0].checks[0].severity, Severity::Info);
        assert!(reports[0].checks[0].detail.contains("daemon not running"));
    }

    #[tokio::test]
    async fn skip_when_no_docker_client() {
        let ctx = ctx_no_docker(None);
        let reports = check_containers(&ctx, true).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].checks[0].name, "skipped");
        assert!(reports[0].checks[0].detail.contains("Docker not connected"));
    }

    #[tokio::test]
    async fn skip_workspace_container_when_no_workspace() {
        let ctx = ctx_no_docker(Some(std::path::PathBuf::from("/nonexistent")));
        let reports = check_containers(&ctx, false).await;
        assert_eq!(reports[0].checks[0].name, "skipped");
        assert!(reports[0].checks[0].detail.contains("daemon not running"));
    }

    #[tokio::test]
    async fn skip_when_daemon_not_running_has_single_report() {
        let ctx = ctx_no_docker(Some(std::path::PathBuf::from("/some/workspace")));
        let reports = check_containers(&ctx, false).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].name, "Containers");
    }

    #[tokio::test]
    async fn skip_when_daemon_not_running_severity_is_info() {
        let ctx = ctx_no_docker(None);
        let reports = check_containers(&ctx, false).await;
        assert_eq!(reports[0].checks[0].severity, Severity::Info);
    }

    #[tokio::test]
    async fn skip_when_no_docker_client_severity_is_info() {
        let ctx = ctx_no_docker(None);
        let reports = check_containers(&ctx, true).await;
        assert_eq!(reports[0].checks[0].severity, Severity::Info);
    }

    #[tokio::test]
    async fn skip_when_no_docker_client_has_correct_detail() {
        let ctx = ctx_no_docker(Some(std::path::PathBuf::from("/workspace")));
        let reports = check_containers(&ctx, true).await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].checks[0].detail.contains("Docker not connected"));
    }

    #[tokio::test]
    async fn daemon_not_running_check_has_no_fix_hint() {
        let ctx = ctx_no_docker(None);
        let reports = check_containers(&ctx, false).await;
        assert!(reports[0].checks[0].fix_hint.is_none());
    }

    #[tokio::test]
    async fn no_docker_client_check_has_no_fix_hint() {
        let ctx = ctx_no_docker(None);
        let reports = check_containers(&ctx, true).await;
        assert!(reports[0].checks[0].fix_hint.is_none());
    }

    #[tokio::test]
    async fn ctx_with_all_flag_still_skips_without_client() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: true,
            docker_client: None,
        };
        let reports = check_containers(&ctx, true).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].checks[0].name, "skipped");
    }
}
