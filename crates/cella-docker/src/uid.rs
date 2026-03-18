//! Update remote user UID/GID to match host user.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tracing::{debug, info};

use crate::CellaDockerError;
use crate::client::DockerClient;
use crate::exec::ExecOptions;

/// Update the remote user's UID/GID inside the container to match the host.
///
/// # Errors
///
/// Returns `CellaDockerError` if the initial UID lookup fails.
/// `usermod`/`groupmod`/`chown` failures are logged but not fatal.
#[allow(clippy::similar_names)]
pub async fn update_remote_user_uid(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    workspace_root: &Path,
) -> Result<(), CellaDockerError> {
    let metadata = std::fs::metadata(workspace_root)?;
    let host_uid = metadata.uid();
    let host_gid = metadata.gid();

    debug!("Host UID: {host_uid}, GID: {host_gid}");

    // Get container user's current UID
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["id".to_string(), "-u".to_string(), remote_user.to_string()],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    let container_uid: u32 = result.stdout.trim().parse().unwrap_or(0);

    if container_uid == host_uid {
        debug!("UID already matches ({host_uid}), skipping remap");
        return Ok(());
    }

    info!("Remapping {remote_user} UID {container_uid} -> {host_uid}");

    // Get primary group name
    let group_result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["id".to_string(), "-gn".to_string(), remote_user.to_string()],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;
    let group_name = group_result.stdout.trim().to_string();

    // Update UID
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "usermod".to_string(),
                    "-u".to_string(),
                    host_uid.to_string(),
                    remote_user.to_string(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    // Update GID
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "groupmod".to_string(),
                    "-g".to_string(),
                    host_gid.to_string(),
                    group_name,
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    // Fix home directory ownership
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "chown".to_string(),
                    "-R".to_string(),
                    format!("{host_uid}:{host_gid}"),
                    format!("/home/{remote_user}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    info!("UID/GID remapping complete");
    Ok(())
}
