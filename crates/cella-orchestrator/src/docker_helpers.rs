//! Docker container lookup and exec helpers.
//!
//! These replace the subprocess-based helpers in the daemon's control server
//! with direct bollard API calls via [`DockerClient`].

use cella_docker::DockerClient;

use crate::error::OrchestratorError;
use crate::result::WorktreeStatus;

/// Find a running container for a given branch by Docker label lookup.
///
/// Queries cella-managed containers and matches the `dev.cella.branch` label.
/// Returns the first matching container name.
///
/// # Errors
///
/// Returns an error if the Docker API query fails.
pub async fn find_container_for_branch(
    client: &DockerClient,
    branch: &str,
) -> Result<Option<String>, OrchestratorError> {
    let containers = client
        .list_cella_containers(false)
        .await
        .map_err(|e| OrchestratorError::Docker {
            message: format!("failed to list containers: {e}"),
        })?;

    Ok(containers.into_iter().find(|c| {
        c.labels.get("dev.cella.worktree").is_some_and(|v| v == "true")
            && c.labels.get("dev.cella.branch").is_some_and(|v| v == branch)
    }).map(|c| c.name))
}

/// List all worktrees with their container status.
///
/// Combines `git worktree list` output with Docker container label queries
/// to produce a unified view of worktrees and their associated containers.
///
/// # Errors
///
/// Returns an error if git or Docker operations fail.
pub async fn worktree_list(
    repo_root: &std::path::Path,
    client: &DockerClient,
) -> Result<Vec<WorktreeStatus>, OrchestratorError> {
    let worktrees = cella_git::list(repo_root).map_err(|e| OrchestratorError::Git {
        message: format!("failed to list worktrees: {e}"),
    })?;

    let cella_containers = client
        .list_cella_containers(false)
        .await
        .map_err(|e| OrchestratorError::Docker {
            message: format!("failed to list containers: {e}"),
        })?;

    let mut results = Vec::with_capacity(worktrees.len());
    for wt in worktrees {
        let wt_path_str = wt.path.to_string_lossy().to_string();

        let container = cella_containers.iter().find(|c| {
            c.labels
                .get("dev.cella.workspace_path")
                .is_some_and(|p| p == &wt_path_str)
        });

        results.push(WorktreeStatus {
            path: wt.path,
            branch: wt.branch,
            is_main: wt.is_main,
            container_name: container.map(|c| c.name.clone()),
            container_state: container.map(|c| format!("{:?}", c.state).to_lowercase()),
        });
    }

    Ok(results)
}

/// Execute a command in a running container.
///
/// # Errors
///
/// Returns an error if the Docker exec fails.
pub async fn container_exec(
    client: &DockerClient,
    container_name: &str,
    cmd: &[String],
    user: Option<&str>,
    working_dir: Option<&str>,
) -> Result<crate::result::ExecResult, OrchestratorError> {
    let result = client
        .exec_command(
            container_name,
            &cella_docker::ExecOptions {
                cmd: cmd.to_vec(),
                user: user.map(String::from),
                env: None,
                working_dir: working_dir.map(String::from),
            },
        )
        .await
        .map_err(|e| OrchestratorError::Docker {
            message: format!("exec failed: {e}"),
        })?;

    Ok(crate::result::ExecResult {
        exit_code: i32::try_from(result.exit_code).unwrap_or(-1),
    })
}

/// Verify that a container is running.
///
/// # Errors
///
/// Returns an error if the container is not running or the check fails.
pub async fn verify_container_running(
    client: &DockerClient,
    container_id: &str,
) -> Result<(), OrchestratorError> {
    let info = client
        .inspect_container(container_id)
        .await
        .map_err(|e| OrchestratorError::Docker {
            message: format!("inspect failed: {e}"),
        })?;

    if info.state != cella_backend::types::ContainerState::Running {
        return Err(OrchestratorError::ContainerExited {
            message: format!(
                "container {container_id} is not running (state: {:?})",
                info.state
            ),
        });
    }

    Ok(())
}
