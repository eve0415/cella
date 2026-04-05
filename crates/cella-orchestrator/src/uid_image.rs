//! Build-time UID/GID remapping via a thin Docker image layer.
//!
//! Matches the devcontainer CLI's `updateUID.Dockerfile` approach: builds a
//! new image layer on top of the base image that updates `/etc/passwd`,
//! `/etc/group`, and chowns the home directory so the remote user's UID/GID
//! matches the host user.

use std::collections::HashMap;
use std::path::PathBuf;

use tracing::{debug, info, warn};

use cella_backend::{BuildOptions, ContainerBackend};

use crate::progress::{ProgressSender, format_elapsed};

/// The Dockerfile content for UID remapping.
///
/// Identical to the devcontainer CLI's `scripts/updateUID.Dockerfile`.
const UID_REMAP_DOCKERFILE: &str = r#"ARG BASE_IMAGE
FROM $BASE_IMAGE
USER root
ARG REMOTE_USER
ARG NEW_UID
ARG NEW_GID
SHELL ["/bin/sh", "-c"]
RUN eval $(sed -n "s/${REMOTE_USER}:[^:]*:\([^:]*\):\([^:]*\):[^:]*:\([^:]*\).*/OLD_UID=\1;OLD_GID=\2;HOME_FOLDER=\3/p" /etc/passwd) \
    && eval $(sed -n "s/\([^:]*\):[^:]*:${NEW_UID}:.*/EXISTING_USER=\1/p" /etc/passwd) \
    && eval $(sed -n "s/\([^:]*\):[^:]*:${NEW_GID}:.*/EXISTING_GROUP=\1/p" /etc/group) \
    && if [ -z "$OLD_UID" ]; then echo "Remote user not found in /etc/passwd (${REMOTE_USER})."; \
       elif [ "$OLD_UID" = "$NEW_UID" ] && [ "$OLD_GID" = "$NEW_GID" ]; then echo "UIDs and GIDs are the same (${NEW_UID}:${NEW_GID})."; \
       elif [ "$OLD_UID" != "$NEW_UID" ] && [ -n "$EXISTING_USER" ]; then echo "User with UID exists ($EXISTING_USER=${NEW_UID})."; \
       else \
         if [ "$OLD_GID" != "$NEW_GID" ] && [ -n "$EXISTING_GROUP" ]; then \
           echo "Group with GID exists ($EXISTING_GROUP=${NEW_GID})."; \
           NEW_GID="$OLD_GID"; \
         fi; \
         echo "Updating UID:GID from $OLD_UID:$OLD_GID to $NEW_UID:$NEW_GID."; \
         sed -i -e "s/\(${REMOTE_USER}:[^:]*:\)[^:]*:[^:]*/\1${NEW_UID}:${NEW_GID}/" /etc/passwd; \
         if [ "$OLD_GID" != "$NEW_GID" ]; then \
           sed -i -e "s/\([^:]*:[^:]*:\)${OLD_GID}:/\1${NEW_GID}:/" /etc/group; \
         fi; \
         chown -R "$NEW_UID:$NEW_GID" "$HOME_FOLDER"; \
       fi
ARG IMAGE_USER=root
USER $IMAGE_USER
"#;

/// Build a UID-remapped image on top of `base_image`.
///
/// Returns `Ok(Some(uid_image_name))` when a new image was built, or
/// `Ok(None)` when remapping was skipped (root user, UID 0, etc.).
///
/// On build failure, logs a warning and returns `Ok(None)` so the caller
/// can fall back to the unmodified base image.
///
/// # Errors
///
/// Returns an error if the Dockerfile cannot be written to disk.
pub async fn build_uid_remap_image(
    client: &dyn ContainerBackend,
    base_image: &str,
    image_user: &str,
    remote_user: &str,
    progress: &ProgressSender,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    if remote_user == "root" {
        debug!("Skipping UID remap: remote user is root");
        return Ok(None);
    }

    let host_uid = nix::unistd::getuid().as_raw();
    let host_gid = nix::unistd::getgid().as_raw();

    if host_uid == 0 {
        debug!("Skipping UID remap: host UID is 0 (root)");
        return Ok(None);
    }

    let uid_image = format!("{base_image}-uid");

    let context_dir = uid_context_dir()?;
    let dockerfile_path = context_dir.join("Dockerfile.uid-remap");
    std::fs::write(&dockerfile_path, UID_REMAP_DOCKERFILE)?;

    let mut args = HashMap::new();
    args.insert("BASE_IMAGE".to_string(), base_image.to_string());
    args.insert("REMOTE_USER".to_string(), remote_user.to_string());
    args.insert("NEW_UID".to_string(), host_uid.to_string());
    args.insert("NEW_GID".to_string(), host_gid.to_string());
    args.insert("IMAGE_USER".to_string(), image_user.to_string());

    let build_opts = BuildOptions {
        image_name: uid_image.clone(),
        context_path: context_dir,
        dockerfile: "Dockerfile.uid-remap".to_string(),
        args,
        target: None,
        cache_from: vec![],
        options: vec![],
        secrets: vec![],
    };

    info!("Building UID remap image: {uid_image} (UID {host_uid}:{host_gid})");
    let start = std::time::Instant::now();
    progress.println("  \x1b[36m▸\x1b[0m Updating remote user UID...");
    let result = client.build_image(&build_opts).await;
    let elapsed_str = format_elapsed(start.elapsed());

    match result {
        Ok(_) => {
            progress.println(&format!(
                "  \x1b[32m✓\x1b[0m Updated remote user UID{elapsed_str}"
            ));
            Ok(Some(uid_image))
        }
        Err(e) => {
            warn!("UID remap image build failed, using base image: {e}");
            progress.println(&format!("  \x1b[33m⚠\x1b[0m UID remap skipped: {e}"));
            Ok(None)
        }
    }
}

/// Return (and create if needed) the directory for UID remap build context.
fn uid_context_dir() -> Result<PathBuf, std::io::Error> {
    let dir = std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join(".cella")
        .join("uid");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dockerfile_template_contains_key_commands() {
        assert!(UID_REMAP_DOCKERFILE.contains("FROM $BASE_IMAGE"));
        assert!(UID_REMAP_DOCKERFILE.contains("USER root"));
        assert!(UID_REMAP_DOCKERFILE.contains("ARG REMOTE_USER"));
        assert!(UID_REMAP_DOCKERFILE.contains("ARG NEW_UID"));
        assert!(UID_REMAP_DOCKERFILE.contains("ARG NEW_GID"));
        assert!(UID_REMAP_DOCKERFILE.contains("chown -R"));
        assert!(UID_REMAP_DOCKERFILE.contains("ARG IMAGE_USER=root"));
        assert!(UID_REMAP_DOCKERFILE.contains("/etc/passwd"));
        assert!(UID_REMAP_DOCKERFILE.contains("/etc/group"));
    }

    #[test]
    fn uid_image_name_appends_suffix() {
        let base = "myimage:latest";
        let uid_name = format!("{base}-uid");
        assert_eq!(uid_name, "myimage:latest-uid");
    }
}
