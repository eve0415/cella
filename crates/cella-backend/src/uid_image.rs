//! Build-time UID/GID remapping via a thin Docker image layer.
//!
//! Matches the devcontainer CLI's `updateUID.Dockerfile` approach: builds a
//! new image layer on top of the base image that updates `/etc/passwd`,
//! `/etc/group`, and chowns the home directory so the remote user's UID/GID
//! matches the host user.

use std::collections::HashMap;
use std::path::PathBuf;

use tracing::{debug, info, warn};

use crate::progress::{ProgressSender, format_elapsed};
use crate::traits::ContainerBackend;
use crate::types::BuildOptions;

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

/// Toolchain inputs threaded into the UID-remap build so it honors the same
/// `--docker-path` / `--buildkit` selection as every other build site.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildToolchain<'a> {
    /// `docker` CLI binary path (`None` = `docker`).
    pub docker_path: Option<&'a str>,
    /// Whether `BuildKit`/buildx may be used (`false` = classic builder).
    pub use_buildkit: bool,
}

/// Whether UID remap should be skipped based on the resolved remote user.
///
/// Mirrors official's post-gate skip (`containerFeatures.ts`): skip when the
/// remote user is `root` or an all-numeric UID (`/^\d+$/`) — there is nothing
/// to remap in either case.
fn skip_remap_for_remote_user(remote_user: &str) -> bool {
    remote_user == "root"
        || (!remote_user.is_empty() && remote_user.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether UID remap is unsupported on the given host OS.
///
/// The official CLI only remaps on Linux: on macOS/Windows the Docker Desktop
/// VM mediates bind-mount file ownership, so there is no host UID to align the
/// container user with (`updateRemoteUserUIDOnMacOS` is hardcoded `false`).
fn remap_unsupported_on_host(host_os: &str) -> bool {
    host_os != "linux"
}

/// Compute the platform string from image inspect fields.
///
/// Joins non-empty fields with `/`: `linux/amd64` or `linux/arm64/v8`.
/// Returns `None` when os is absent or empty (platform cannot be determined).
pub fn image_platform(
    os: Option<&str>,
    arch: Option<&str>,
    variant: Option<&str>,
) -> Option<String> {
    let os = os.filter(|s| !s.is_empty())?;
    let parts: Vec<&str> = std::iter::once(os)
        .chain(arch.into_iter().filter(|s| !s.is_empty()))
        .chain(variant.into_iter().filter(|s| !s.is_empty()))
        .collect();
    Some(parts.join("/"))
}

/// Build a UID-remapped image on top of `base_image`.
///
/// Returns `Ok(Some(uid_image_name))` when a new image was built, or
/// `Ok(None)` when remapping was skipped for any of the following reasons:
/// - The host OS is not Linux (Docker Desktop's VM mediates file ownership
///   on macOS/Windows, so there is no host UID to align with).
/// - The remote user is `root` or an all-numeric UID.
/// - The host UID is 0 (root).
///
/// On build failure, logs a warning and returns `Ok(None)` so the caller
/// can fall back to the unmodified base image.
///
/// The platform (`--platform`) is derived by inspecting `base_image` after
/// the skip guards pass. On inspect failure the flag is omitted, which is
/// byte-identical to the pre-platform behaviour.
///
/// # Errors
///
/// Returns an error if the Dockerfile cannot be written to disk.
pub async fn build_uid_remap_image(
    client: &dyn ContainerBackend,
    base_image: &str,
    image_user: &str,
    remote_user: &str,
    toolchain: BuildToolchain<'_>,
    progress: &ProgressSender,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    if remap_unsupported_on_host(std::env::consts::OS) {
        debug!(
            "Skipping UID remap: host OS is {} (Docker Desktop mediates file ownership off Linux)",
            std::env::consts::OS
        );
        return Ok(None);
    }

    if skip_remap_for_remote_user(remote_user) {
        debug!("Skipping UID remap: remote user is root or numeric ({remote_user})");
        return Ok(None);
    }

    let host_uid = nix::unistd::getuid().as_raw();
    let host_gid = nix::unistd::getgid().as_raw();

    if host_uid == 0 {
        debug!("Skipping UID remap: host UID is 0 (root)");
        return Ok(None);
    }

    // Inspect the base image to derive `--platform`. Failures are non-fatal:
    // omitting `--platform` is byte-identical to the pre-platform behaviour.
    let platform = client
        .inspect_image_details(base_image)
        .await
        .ok()
        .and_then(|d| {
            image_platform(
                d.os.as_deref(),
                d.architecture.as_deref(),
                d.variant.as_deref(),
            )
        });

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
        cache_to: None,
        options: vec![],
        secrets: vec![],
        use_buildkit: toolchain.use_buildkit,
        docker_path: toolchain.docker_path.map(str::to_string),
        platform,
        // The UID-remap build is a post-build transform on an already-built
        // image, never the `--output` export target — keep the default --load.
        output: None,
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

    #[test]
    fn skip_remap_for_root_and_numeric_users() {
        assert!(skip_remap_for_remote_user("root"));
        assert!(skip_remap_for_remote_user("1000"));
        assert!(skip_remap_for_remote_user("0"));
    }

    #[test]
    fn no_skip_for_named_user() {
        assert!(!skip_remap_for_remote_user("vscode"));
        assert!(!skip_remap_for_remote_user("node"));
        // Empty string is not numeric per /^\d+$/ (requires >=1 digit).
        assert!(!skip_remap_for_remote_user(""));
    }

    #[test]
    fn remap_only_on_linux_host() {
        assert!(!remap_unsupported_on_host("linux"));
        // Off Linux, Docker Desktop's VM handles bind-mount ownership.
        assert!(remap_unsupported_on_host("macos"));
        assert!(remap_unsupported_on_host("windows"));
    }

    // -----------------------------------------------------------------------
    // image_platform
    // -----------------------------------------------------------------------

    #[test]
    fn image_platform_linux_amd64() {
        assert_eq!(
            image_platform(Some("linux"), Some("amd64"), None),
            Some("linux/amd64".to_string())
        );
    }

    #[test]
    fn image_platform_linux_arm64_v8() {
        assert_eq!(
            image_platform(Some("linux"), Some("arm64"), Some("v8")),
            Some("linux/arm64/v8".to_string())
        );
    }

    #[test]
    fn image_platform_missing_os_returns_none() {
        assert_eq!(image_platform(None, Some("amd64"), None), None);
    }

    #[test]
    fn image_platform_empty_os_returns_none() {
        assert_eq!(image_platform(Some(""), Some("amd64"), None), None);
    }

    #[test]
    fn image_platform_os_only() {
        assert_eq!(
            image_platform(Some("linux"), None, None),
            Some("linux".to_string())
        );
    }

    #[test]
    fn image_platform_empty_arch_skipped_no_trailing_slash() {
        // OS present, ARCH is an empty string — must be filtered, not joined.
        // Expected: "linux", not "linux/".
        assert_eq!(
            image_platform(Some("linux"), Some(""), None),
            Some("linux".to_string())
        );
    }
}
