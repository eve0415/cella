//! Git SSH signing allowed-signers forwarding.
//!
//! Git needs `gpg.ssh.allowedSignersFile` to point at a file that exists
//! inside the container, or every signature verification fails with
//! `gpg.ssh.allowedSignersFile needs to be configured and exist`.
//! Forwarding the host's config value alone leaves a dangling host path,
//! so the file itself is copied and the forwarded value is rewritten.

use std::path::{Path, PathBuf};

use crate::FileUpload;
use crate::ssh_config::remote_ssh_dir;

/// Filename of the allowed-signers file inside the container.
///
/// Fixed regardless of where the file lives on the host, matching the
/// convention the VS Code dev containers extension uses.
const CONTAINER_FILENAME: &str = "allowed_signers";

/// Arguments used to resolve the host's allowed-signers file path.
///
/// `--type=path` expands a leading `~`, which git otherwise reports
/// verbatim; `--includes` is needed because `--global` turns
/// include-following off when it selects a specific config file.
const RESOLVE_ARGS: [&str; 6] = [
    "config",
    "--global",
    "--includes",
    "--type=path",
    "--get",
    "gpg.ssh.allowedSignersFile",
];

/// Container-side path of the forwarded allowed-signers file.
pub fn container_allowed_signers_path(remote_user: &str) -> String {
    format!("{}/{CONTAINER_FILENAME}", remote_ssh_dir(remote_user))
}

/// Resolve the host's configured allowed-signers file path.
///
/// Returns `None` when git is missing or the key is unset.
fn resolve_host_path() -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(RESOLVE_ARGS)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    if path.is_empty() {
        return None;
    }

    Some(PathBuf::from(path))
}

/// Build the container upload for a host allowed-signers file.
///
/// Returns `None` when the file is missing, unreadable, or empty.
/// Content is copied verbatim — every principal the host trusts.
fn build_upload(host_path: &Path, remote_user: &str) -> Option<FileUpload> {
    let content = match std::fs::read(host_path) {
        Ok(content) if content.is_empty() => return None,
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                "Failed to read allowed signers file at {}: {e}",
                host_path.display()
            );
            return None;
        }
    };

    Some(FileUpload {
        container_path: container_allowed_signers_path(remote_user),
        content,
        mode: 0o600,
    })
}

/// Read the host's allowed-signers file as a container upload.
///
/// Returns `None` when SSH signing is not configured on the host or the
/// configured file cannot be read, in which case nothing is forwarded.
pub fn read_allowed_signers_upload(remote_user: &str) -> Option<FileUpload> {
    let host_path = resolve_host_path()?;
    build_upload(&host_path, remote_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn container_path_root() {
        assert_eq!(
            container_allowed_signers_path("root"),
            "/root/.ssh/allowed_signers"
        );
    }

    #[test]
    fn container_path_regular_user() {
        assert_eq!(
            container_allowed_signers_path("node"),
            "/home/node/.ssh/allowed_signers"
        );
    }

    #[test]
    fn upload_missing_file_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let upload = build_upload(&tmp.path().join("absent"), "vscode");
        assert!(upload.is_none(), "a missing file must not be forwarded");
    }

    #[test]
    fn upload_empty_file_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("allowed_signers");
        fs::write(&file, "").unwrap();

        let upload = build_upload(&file, "vscode");
        assert!(upload.is_none(), "an empty file must not be forwarded");
    }

    #[test]
    fn upload_copies_all_principals_verbatim() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("allowed_signers");
        let content = "me@example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE\n\
             other@example.com namespaces=\"git\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAATWO\n";
        fs::write(&file, content).unwrap();

        let upload = build_upload(&file, "vscode").expect("upload should be built");
        assert_eq!(upload.content, content.as_bytes());
        assert_eq!(upload.container_path, "/home/vscode/.ssh/allowed_signers");
        assert_eq!(upload.mode, 0o600);
    }

    #[test]
    fn upload_target_ignores_host_location() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("signers-elsewhere");
        fs::write(&file, "me@example.com ssh-ed25519 AAAA\n").unwrap();

        let upload = build_upload(&file, "root").expect("upload should be built");
        assert_eq!(upload.container_path, "/root/.ssh/allowed_signers");
    }

    #[test]
    fn resolve_args_report_unset_key() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global");
        fs::write(&global, "[user]\n\tname = No Signing\n").unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = std::process::Command::new("git")
            .args(RESOLVE_ARGS)
            .env("GIT_CONFIG_GLOBAL", &global)
            .output()
        else {
            return;
        };
        assert!(
            !output.status.success(),
            "an unset key must not resolve to a path"
        );
        assert!(output.stdout.is_empty(), "an unset key must print nothing");
    }

    #[test]
    fn resolve_args_expand_tilde_and_follow_includes() {
        let tmp = TempDir::new().unwrap();
        let included = tmp.path().join("included");
        fs::write(
            &included,
            "[gpg \"ssh\"]\n\tallowedSignersFile = ~/.config/git/allowed_signers\n",
        )
        .unwrap();
        let global = tmp.path().join("global");
        fs::write(
            &global,
            format!("[include]\n\tpath = {}\n", included.display()),
        )
        .unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = std::process::Command::new("git")
            .args(RESOLVE_ARGS)
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("HOME", tmp.path())
            .output()
        else {
            return;
        };
        assert!(output.status.success(), "git config --get should succeed");

        let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            resolved,
            tmp.path()
                .join(".config/git/allowed_signers")
                .display()
                .to_string(),
            "the resolved path must follow includes and expand ~"
        );
    }
}
