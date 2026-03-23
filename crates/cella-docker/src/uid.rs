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

/// Update the remote user's UID/GID inside the container to match the host.
///
/// # Errors
///
/// Returns `CellaDockerError` if the initial UID lookup fails.
/// The update script itself is best-effort; failures are logged but not fatal.
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

    // Quick check: if the container UID already matches, skip everything.
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

    // Run the update script matching the original devcontainer CLI behaviour.
    let script = build_update_uid_script(remote_user, host_uid, host_gid);
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), script],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    match result {
        Ok(r) => {
            for line in r.stdout.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    info!("{line}");
                }
            }
            if r.exit_code != 0 && !r.stderr.is_empty() {
                warn!("UID update script stderr: {}", r.stderr.trim());
            }
        }
        Err(e) => {
            warn!("Failed to run UID update script: {e}");
        }
    }

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
