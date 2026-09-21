//! Per-container health checks.

use std::collections::HashMap;

use cella_backend::{ContainerBackend, ContainerTarget, ExecOptions};
use cella_protocol::{ContainerProbeResult, ManagementRequest, ManagementResponse};

use super::{CHECK_TIMEOUT, CategoryReport, CheckContext, CheckResult, Severity};

/// Budget for the whole container category.
///
/// Held under [`CHECK_TIMEOUT`] so a slow host yields the containers that were
/// checked plus a note, rather than the category timing out and discarding
/// everything it had.
const CONTAINER_BUDGET: std::time::Duration =
    CHECK_TIMEOUT.saturating_sub(std::time::Duration::from_millis(500));

/// How long to wait for the daemon to answer a reachability probe.
///
/// The daemon bounds the probe itself, but the request to it is not bounded,
/// and a daemon that has stopped answering is one of the things doctor is run
/// to find out about.
const PROBE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Run container diagnostics.
///
/// Returns one `CategoryReport` per container, or a single report
/// explaining why container checks were skipped.
pub async fn check_containers(ctx: &CheckContext, daemon_running: bool) -> Vec<CategoryReport> {
    // Started before discovery, because the category timeout this stays under
    // is already running by the time listing the containers begins.
    let deadline = std::time::Instant::now() + CONTAINER_BUDGET;
    // Only require a running daemon for backends with managed agents.
    // Unmanaged backends (e.g. Apple Container) can still run basic
    // container checks (running state, version skew) without the daemon.
    let needs_daemon = ctx
        .backend_client
        .as_ref()
        .is_none_or(|c| c.capabilities().managed_agent);

    if !daemon_running && needs_daemon {
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

    let Some(ref client) = ctx.backend_client else {
        return vec![CategoryReport::new(
            "Containers",
            vec![CheckResult {
                name: "skipped".into(),
                severity: Severity::Info,
                detail: "container checks skipped: selected backend not connected".into(),
                fix_hint: None,
            }],
        )];
    };

    if ctx.all {
        check_all_containers(client.as_ref(), deadline).await
    } else {
        check_workspace_container(ctx, client.as_ref(), deadline).await
    }
}

async fn check_all_containers(
    client: &dyn ContainerBackend,
    deadline: std::time::Instant,
) -> Vec<CategoryReport> {
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
            let ids = containers.iter().map(|c| c.id.clone()).collect();

            // The probes and the per-container checks are independent, so the
            // probes run while the checks do rather than in front of them.
            // Both arms are bounded by what is left of the budget, so neither
            // can hold the category past the point where its results are lost.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let (mut probes, collected) = tokio::join!(probe_containers(ids, remaining), async {
                let mut collected: Vec<Vec<CheckResult>> = Vec::new();
                for container in &containers {
                    if std::time::Instant::now() >= deadline {
                        collected.push(vec![CheckResult {
                            name: "skipped".into(),
                            severity: Severity::Info,
                            detail: "ran out of time before this container was checked".into(),
                            fix_hint: Some(
                                "Check one workspace at a time with `cella doctor`".into(),
                            ),
                        }]);
                        continue;
                    }
                    // Bounded per container: an unresponsive host otherwise
                    // never returns here, and the deadline above is only
                    // consulted between containers.
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let checks = tokio::time::timeout(
                        remaining,
                        check_single_container(client, &container.id),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        vec![CheckResult {
                            name: "checks".into(),
                            severity: Severity::Warning,
                            detail: "this container stopped responding partway through".into(),
                            fix_hint: None,
                        }]
                    });
                    collected.push(checks);
                }
                collected
            });

            // The category's status is derived from its checks, so the probe
            // has to be merged before the report is built or an unreachable
            // container is summarised as passing.
            containers
                .iter()
                .zip(collected)
                .map(|(container, mut checks)| {
                    checks.extend(probes.remove(&container.id));
                    CategoryReport::new(format!("Container: {}", container.name), checks)
                })
                .collect()
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
    client: &dyn ContainerBackend,
    deadline: std::time::Instant,
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

    let target = ContainerTarget {
        container_id: None,
        container_name: None,
        id_labels: Vec::new(),
        workspace_folder: Some(workspace.clone()),
    };

    match target.resolve(client, false).await {
        Ok(container) => {
            let name = format!("Container: {}", container.name);
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return vec![CategoryReport::new(
                    name,
                    vec![CheckResult {
                        name: "skipped".into(),
                        severity: Severity::Info,
                        detail: "ran out of time before this container was checked".into(),
                        fix_hint: None,
                    }],
                )];
            }

            let (checks, probe) = tokio::join!(
                tokio::time::timeout(remaining, check_single_container(client, &container.id)),
                check_host_can_reach_container(&container.id, remaining)
            );
            let mut checks = checks.unwrap_or_else(|_| {
                vec![CheckResult {
                    name: "checks".into(),
                    severity: Severity::Warning,
                    detail: "this container stopped responding partway through".into(),
                    fix_hint: None,
                }]
            });
            checks.extend(probe);
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
    client: &dyn ContainerBackend,
    container_id: &str,
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

    // Agent/port checks only apply to backends with managed agents
    if client.capabilities().managed_agent {
        // Agent connectivity via daemon
        check_agent_connectivity(&mut checks, container_id).await;

        // Credential forwarding
        check_credentials(client, container_id, &mut checks).await;

        // Port forwarding
        check_ports(&mut checks, container_id).await;
    }

    checks
}

async fn check_version_skew(
    client: &dyn ContainerBackend,
    container_id: &str,
    checks: &mut Vec<CheckResult>,
) {
    let cli_version = env!("CARGO_PKG_VERSION");

    // Prefer the live agent version from the daemon handshake.
    // Fall back to the Docker label when the agent isn't connected
    // (unmanaged backend, agent crashed, etc.).
    let agent_version = query_live_agent_version(container_id).await;

    match agent_version.as_deref() {
        Some(v) if v == cli_version => {
            checks.push(CheckResult {
                name: "version".into(),
                severity: Severity::Pass,
                detail: cli_version.to_string(),
                fix_hint: None,
            });
        }
        Some(v) => {
            checks.push(CheckResult {
                name: "version".into(),
                severity: Severity::Warning,
                detail: format!("agent {v} != CLI {cli_version}"),
                fix_hint: Some("Run `cella up` to update the agent".into()),
            });
        }
        None => {
            // Fall back to Docker label for unmanaged backends or
            // disconnected agents.
            let label_version = client
                .inspect_container(container_id)
                .await
                .ok()
                .and_then(|info| info.labels.get("dev.cella.version").cloned());
            let container_version = label_version.as_deref().unwrap_or("unknown");
            if container_version == cli_version {
                checks.push(CheckResult {
                    name: "version".into(),
                    severity: Severity::Pass,
                    detail: cli_version.to_string(),
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
    }
}

/// Query the daemon for a container's live agent version.
async fn query_live_agent_version(container_id: &str) -> Option<String> {
    let mgmt_socket = cella_env::paths::daemon_socket_path()?;
    if !mgmt_socket.exists() {
        return None;
    }

    let resp =
        cella_daemon_client::send_management_request(&mgmt_socket, &ManagementRequest::QueryStatus)
            .await
            .ok()?;

    if let ManagementResponse::Status { containers, .. } = resp {
        containers
            .into_iter()
            .find(|c| c.container_id == container_id && c.agent_connected)
            .and_then(|c| c.agent_version)
    } else {
        None
    }
}

async fn check_agent_connectivity(checks: &mut Vec<CheckResult>, container_id: &str) {
    let Some(mgmt_socket) = cella_env::paths::daemon_socket_path() else {
        return;
    };

    match cella_daemon_client::send_management_request(
        &mgmt_socket,
        &ManagementRequest::QueryStatus,
    )
    .await
    {
        Ok(ManagementResponse::Status { containers, .. }) => {
            let found = containers
                .iter()
                .any(|c| c.container_id == container_id && c.agent_connected);
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
    client: &dyn ContainerBackend,
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
            &ExecOptions {
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

/// Check that the daemon can still open a connection to the container.
///
/// A forward binds its host port whether or not the container is reachable, so
/// the port count above stays reassuring while every connection is dropped
/// upstream. The daemon answers this because it is the process that performs
/// the connection user traffic depends on.
async fn check_host_can_reach_container(
    container_id: &str,
    budget: std::time::Duration,
) -> Option<CheckResult> {
    let mgmt_socket = cella_env::paths::daemon_socket_path()?;
    let client = cella_daemon_client::DaemonClient::new(mgmt_socket);

    let Ok(answer) = tokio::time::timeout(budget, client.probe_container(container_id)).await
    else {
        return Some(CheckResult {
            name: "container reachable from host".into(),
            severity: Severity::Warning,
            detail: "the daemon did not answer a reachability probe in time".into(),
            fix_hint: Some("Check the daemon with `cella daemon status`".into()),
        });
    };
    let result = answer.ok()?;

    let (severity, detail, fix_hint) = match result {
        ContainerProbeResult::Reachable { detail } => (Severity::Pass, detail, None),
        ContainerProbeResult::Unreachable { error } => (
            Severity::Error,
            format!("{error}; forwarded ports will accept connections and then drop them"),
            Some(
                "Check that the container runtime still routes to the container, and that no \
                 firewall or network policy blocks the cella daemon."
                    .to_string(),
            ),
        ),
        ContainerProbeResult::NotApplicable { reason }
        | ContainerProbeResult::Unknown { reason } => (Severity::Info, reason, None),
    };

    Some(CheckResult {
        name: "container reachable from host".into(),
        severity,
        detail,
        fix_hint,
    })
}

/// Probe every container at once.
///
/// The container category has a fixed time budget for all of its checks, and a
/// probe that goes unanswered costs the full timeout. Running them together
/// keeps the cost of several unreachable containers at roughly one probe rather
/// than one per container, which is what stops the whole category from being
/// discarded exactly when it has something to report.
async fn probe_containers(
    container_ids: Vec<String>,
    budget: std::time::Duration,
) -> HashMap<String, CheckResult> {
    // Bounding each probe rather than the set means one container going quiet
    // costs its own result, not everyone else's.
    let per_probe = budget.min(PROBE_REQUEST_TIMEOUT);
    let mut probes = tokio::task::JoinSet::new();
    for id in container_ids {
        probes.spawn(async move {
            let check = check_host_can_reach_container(&id, per_probe).await;
            (id, check)
        });
    }

    let mut results = HashMap::new();
    while let Some(joined) = probes.join_next().await {
        if let Ok((id, Some(check))) = joined {
            results.insert(id, check);
        }
    }
    results
}

async fn check_ports(checks: &mut Vec<CheckResult>, container_id: &str) {
    let Some(mgmt_socket) = cella_env::paths::daemon_socket_path() else {
        return;
    };

    match cella_daemon_client::send_management_request(
        &mgmt_socket,
        &ManagementRequest::QueryStatus,
    )
    .await
    {
        Ok(ManagementResponse::Status { containers, .. }) => {
            let port_count = containers
                .iter()
                .find(|c| c.container_id == container_id)
                .map_or(0, |c| c.forwarded_port_count);
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
            backend_kind: None,
            backend_client: None,
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
        assert!(
            reports[0].checks[0]
                .detail
                .contains("backend not connected")
        );
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
        assert!(
            reports[0].checks[0]
                .detail
                .contains("backend not connected")
        );
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
            backend_kind: None,
            backend_client: None,
        };
        let reports = check_containers(&ctx, true).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].checks[0].name, "skipped");
    }
}
