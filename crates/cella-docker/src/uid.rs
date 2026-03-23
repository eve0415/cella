//! Update remote user UID/GID to match host user.
//!
//! Matches the original devcontainer CLI's `updateUID.Dockerfile` logic: uses
//! `sed` to atomically update both UID and GID in `/etc/passwd`, handles edge
//! cases (existing user with target UID, existing group with target GID), and
//! chowns the home directory.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tracing::{debug, info, warn};

use crate::CellaDockerError;
use crate::client::DockerClient;
use crate::exec::ExecOptions;

/// Build the shell script that updates UID/GID in `/etc/passwd` and `/etc/group`.
///
/// Ported from the original devcontainer CLI's `scripts/updateUID.Dockerfile`.
fn build_update_uid_script(remote_user: &str, new_uid: u32, new_gid: u32) -> String {
    let header = format!("REMOTE_USER='{remote_user}'\nNEW_UID='{new_uid}'\nNEW_GID='{new_gid}'");

    // The rest is a static POSIX shell script that references $REMOTE_USER,
    // $NEW_UID, $NEW_GID as shell variables.  Kept as a raw string so that
    // sed back-references (`\1`) and shell expansions (`${…}`) pass through
    // without Rust format-string interference.
    let body = r#"
eval $(sed -n "s/${REMOTE_USER}:[^:]*:\([^:]*\):\([^:]*\):[^:]*:\([^:]*\).*/OLD_UID=\1;OLD_GID=\2;HOME_FOLDER=\3/p" /etc/passwd)
eval $(sed -n "s/\([^:]*\):[^:]*:${NEW_UID}:.*/EXISTING_USER=\1/p" /etc/passwd)
eval $(sed -n "s/\([^:]*\):[^:]*:${NEW_GID}:.*/EXISTING_GROUP=\1/p" /etc/group)
if [ -z "$OLD_UID" ]; then
    echo "Remote user not found in /etc/passwd (${REMOTE_USER})."
elif [ "$OLD_UID" = "$NEW_UID" -a "$OLD_GID" = "$NEW_GID" ]; then
    echo "UIDs and GIDs already match (${NEW_UID}:${NEW_GID})."
elif [ "$OLD_UID" != "$NEW_UID" -a -n "$EXISTING_USER" ]; then
    echo "User with UID exists ($EXISTING_USER=${NEW_UID})."
else
    if [ "$OLD_GID" != "$NEW_GID" -a -n "$EXISTING_GROUP" ]; then
        echo "Group with GID exists ($EXISTING_GROUP=${NEW_GID})."
        NEW_GID="$OLD_GID"
    fi
    echo "Updating UID:GID from $OLD_UID:$OLD_GID to $NEW_UID:$NEW_GID."
    sed -i -e "s/\(${REMOTE_USER}:[^:]*:\)[^:]*:[^:]*/\1${NEW_UID}:${NEW_GID}/" /etc/passwd
    if [ "$OLD_GID" != "$NEW_GID" ]; then
        sed -i -e "s/\([^:]*:[^:]*:\)${OLD_GID}:/\1${NEW_GID}:/" /etc/group
    fi
    chown -R "$NEW_UID:$NEW_GID" "$HOME_FOLDER"
fi"#;

    format!("{header}{body}")
}

/// Build [`ExecOptions`] for running a command as root.
fn root_exec(cmd: Vec<String>) -> ExecOptions {
    ExecOptions {
        cmd,
        user: Some("root".to_string()),
        env: None,
        working_dir: None,
    }
}

/// Read the host UID and GID from the workspace root directory metadata.
fn host_ids(workspace_root: &Path) -> Result<(u32, u32), CellaDockerError> {
    let metadata = std::fs::metadata(workspace_root)?;
    Ok((metadata.uid(), metadata.gid()))
}

/// Query the container current UID for the given user.
async fn get_container_uid(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
) -> Result<u32, CellaDockerError> {
    let opts = root_exec(vec![
        "id".to_string(),
        "-u".to_string(),
        remote_user.to_string(),
    ]);
    let result = client.exec_command(container_id, &opts).await?;
    Ok(result.stdout.trim().parse().unwrap_or(0))
}

/// Log the output of a successful UID update script execution.
fn log_uid_update_result(result: &crate::exec::ExecResult) {
    for line in result.stdout.lines() {
        let line = line.trim();
        if !line.is_empty() {
            info!("{line}");
        }
    }
    if result.exit_code != 0 && !result.stderr.is_empty() {
        warn!("UID update script stderr: {}", result.stderr.trim());
    }
}

/// Execute the UID update script and log its output. Best-effort; failures are warned.
async fn run_uid_update_script(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    host_uid: u32,
    host_gid: u32,
) {
    let script = build_update_uid_script(remote_user, host_uid, host_gid);
    let opts = root_exec(vec!["sh".to_string(), "-c".to_string(), script]);

    match client.exec_command(container_id, &opts).await {
        Ok(r) => log_uid_update_result(&r),
        Err(e) => warn!("Failed to run UID update script: {e}"),
    }
}

/// Check if a UID remap is needed by comparing host and container UIDs.
/// Returns `Some((host_uid, host_gid))` if remap is needed, `None` if UIDs already match.
async fn remap_needed(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    workspace_root: &Path,
) -> Result<Option<(u32, u32)>, CellaDockerError> {
    let (host_uid, host_gid) = host_ids(workspace_root)?;
    debug!("Host UID: {host_uid}, GID: {host_gid}");

    let container_uid = get_container_uid(client, container_id, remote_user).await?;

    if container_uid == host_uid {
        debug!("UID already matches ({host_uid}), skipping remap");
        return Ok(None);
    }

    info!("Remapping {remote_user} UID {container_uid} -> {host_uid}");
    Ok(Some((host_uid, host_gid)))
}

/// Update the remote user UID/GID inside the container to match the host.
///
/// # Errors
///
/// Returns `CellaDockerError` if the initial UID lookup fails.
/// The update script itself is best-effort; failures are logged but not fatal.
pub async fn update_remote_user_uid(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    workspace_root: &Path,
) -> Result<(), CellaDockerError> {
    let Some((host_uid, host_gid)) =
        remap_needed(client, container_id, remote_user, workspace_root).await?
    else {
        return Ok(());
    };

    run_uid_update_script(client, container_id, remote_user, host_uid, host_gid).await;
    info!("UID/GID remapping complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_contains_correct_variables() {
        let script = build_update_uid_script("vscode", 501, 501);
        assert!(script.contains("REMOTE_USER='vscode'"));
        assert!(script.contains("NEW_UID='501'"));
        assert!(script.contains("NEW_GID='501'"));
    }

    #[test]
    fn script_contains_sed_and_chown() {
        let script = build_update_uid_script("vscode", 501, 501);
        // sed on /etc/passwd must update BOTH UID and GID
        assert!(script.contains(r#"sed -i -e "s/\(${REMOTE_USER}:[^:]*:\)[^:]*:[^:]*/\1${NEW_UID}:${NEW_GID}/" /etc/passwd"#));
        // sed on /etc/group
        assert!(
            script
                .contains(r#"sed -i -e "s/\([^:]*:[^:]*:\)${OLD_GID}:/\1${NEW_GID}:/" /etc/group"#)
        );
        // chown home directory
        assert!(script.contains(r#"chown -R "$NEW_UID:$NEW_GID" "$HOME_FOLDER""#));
    }

    #[test]
    fn script_handles_different_uid_gid() {
        let script = build_update_uid_script("devuser", 1001, 1002);
        assert!(script.contains("REMOTE_USER='devuser'"));
        assert!(script.contains("NEW_UID='1001'"));
        assert!(script.contains("NEW_GID='1002'"));
    }

    #[test]
    fn script_handles_edge_cases() {
        let script = build_update_uid_script("vscode", 501, 501);
        // Existing user with target UID → skip
        assert!(script.contains(r#"echo "User with UID exists ($EXISTING_USER=${NEW_UID}).""#));
        // Existing group with target GID → keep old GID
        assert!(script.contains(r#"echo "Group with GID exists ($EXISTING_GROUP=${NEW_GID}).""#));
        assert!(script.contains(r#"NEW_GID="$OLD_GID""#));
    }
}
