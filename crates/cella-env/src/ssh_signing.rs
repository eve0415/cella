//! Git SSH signing material forwarding.
//!
//! Two host git config keys under SSH signing each name a file rather than a value.
//! `gpg.ssh.allowedSignersFile` must point at a file that exists inside the container, or every signature verification fails with `gpg.ssh.allowedSignersFile needs to be configured and exist`.
//! A path-valued `user.signingKey` must likewise resolve inside the container, or `git commit -S` fails with `Load key ...: No such file or directory`.
//! Forwarding either host value alone leaves a dangling host path, so each file is copied and the forwarded value is rewritten to the copy.

use std::path::{Path, PathBuf};

use crate::FileUpload;
use crate::git_config::{ALLOWED_SIGNERS_KEY, GPG_FORMAT_KEY, SIGNING_KEY, resolve_last};
use crate::ssh_config::remote_ssh_dir;

/// Filename of the allowed-signers file inside the container.
///
/// Fixed regardless of where the file lives on the host, matching the
/// convention the VS Code dev containers extension uses.
const ALLOWED_SIGNERS_FILENAME: &str = "allowed_signers";

/// Filename of the SSH signing key inside the container.
///
/// Fixed rather than carried over from the host, so that a host key named `config`, `known_hosts` or `allowed_signers` cannot overwrite one of the other files cella writes into the same directory.
/// The `.pub` suffix records that only the public half is ever copied.
const SIGNING_KEY_FILENAME: &str = "signing_key.pub";

/// Marker that opens every PEM-armored key OpenSSH writes.
///
/// An SSH public key file starts with its algorithm name instead, so this
/// separates the two halves of a key pair without parsing either.
const PEM_HEADER: &[u8] = b"-----BEGIN";

/// Arguments that resolve one host config key to a filesystem path.
///
/// `--type=path` expands a leading `~`, which git otherwise reports
/// verbatim; `--includes` is needed because `--global` turns
/// include-following off when it selects a specific config file.
const fn resolve_args(key: &str) -> [&str; 6] {
    [
        "config",
        "--global",
        "--includes",
        "--type=path",
        "--get",
        key,
    ]
}

/// Arguments used to resolve the host's allowed-signers file path.
const RESOLVE_ARGS: [&str; 6] = resolve_args(ALLOWED_SIGNERS_KEY);

/// Arguments used to resolve the host's signing key file path.
const SIGNING_KEY_ARGS: [&str; 6] = resolve_args(SIGNING_KEY);

/// The value the forwarded `user.signingKey` must carry.
///
/// Git reads the host value as either literal key material or a filename, and
/// only the filename form needs anything done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningKeyValue {
    /// Forward the host value unchanged: literal key material, an unset key, or a host that does not sign with SSH.
    Verbatim,
    /// Forward this container path, naming the key copied in, instead of the host path.
    Rewritten(String),
    /// Forward nothing: the host value named a file that could not be copied in.
    Omitted,
}

/// What the host's `user.signingKey` contributes to the container.
#[derive(Debug, Clone)]
pub struct SigningKeyForwarding {
    /// The public key to copy in, when the host value named a readable file.
    pub upload: Option<FileUpload>,
    /// The value the forwarded `user.signingKey` must carry.
    pub value: SigningKeyValue,
}

/// Whether git reads this `user.signingKey` value as literal SSH key material.
///
/// Mirrors `is_literal_ssh_key` in git's `gpg-interface.c`, which accepts a
/// `key::` prefix and — for backward compatibility — a bare `ssh-` prefix, and
/// treats everything else as a filename. Both checks are byte-exact in git, so
/// they are here too: an over-eager match would turn inline key material into a
/// filename that cannot exist.
fn is_literal_key(value: &str) -> bool {
    value.starts_with("key::") || value.starts_with("ssh-")
}

/// Resolve a host config key to a filesystem path.
///
/// Resolved from `workspace_folder` so a value set behind an `includeIf gitdir:` condition is the one the user sees in that repository.
/// Returns `None` when git is missing or the key is unset.
fn resolve_host_path(args: &[&str], workspace_folder: &Path) -> Option<PathBuf> {
    let output = crate::git_config::host_git_output(args, workspace_folder).ok()?;

    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    if path.is_empty() {
        return None;
    }

    // `--type=path` expands `~` but leaves a relative value relative, and git resolves such a value against the repository it runs in, so anchor it there rather than against whatever directory cella itself was started from.
    let path = PathBuf::from(path);
    Some(if path.is_relative() {
        workspace_folder.join(path)
    } else {
        path
    })
}

/// Read a host file that only counts when it has content.
///
/// Returns `None` when the file is missing, unreadable, or empty. `what` names
/// the file in the warning an unreadable file produces.
fn read_non_empty(host_path: &Path, what: &str) -> Option<Vec<u8>> {
    match std::fs::read(host_path) {
        Ok(content) if content.is_empty() => None,
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!("Failed to read {what} at {}: {e}", host_path.display());
            None
        }
    }
}

/// Build the container upload for a host allowed-signers file.
///
/// Returns `None` when the file is missing, unreadable, or empty.
/// Content is copied verbatim — every principal the host trusts.
fn build_upload(host_path: &Path, remote_user: &str) -> Option<FileUpload> {
    let content = read_non_empty(host_path, "allowed signers file")?;

    Some(FileUpload {
        container_path: format!("{}/{ALLOWED_SIGNERS_FILENAME}", remote_ssh_dir(remote_user)),
        content,
        mode: 0o600,
    })
}

/// Read the public half of the host's configured signing key.
///
/// `user.signingKey` may name either half of the pair — git's config
/// documentation says it "can contain the path to either your private ssh key
/// or the public key when ssh-agent is used". Only the public half belongs in
/// the container: `ssh-keygen -Y sign` takes the private half from the
/// forwarded agent, so a value naming the private key is swapped for its
/// conventional `.pub` sibling rather than copying the secret in.
fn read_public_key(host_path: &Path) -> Option<Vec<u8>> {
    let content = read_non_empty(host_path, "signing key")?;
    if !content.starts_with(PEM_HEADER) {
        return Some(content);
    }

    let mut sibling = host_path.as_os_str().to_os_string();
    sibling.push(".pub");
    let sibling = PathBuf::from(sibling);
    match read_non_empty(&sibling, "signing key") {
        Some(public) if !public.starts_with(PEM_HEADER) => Some(public),
        _ => {
            tracing::warn!(
                "Signing key {} holds a private key and {} is not a usable public key, so user.signingkey is not forwarded",
                host_path.display(),
                sibling.display()
            );
            None
        }
    }
}

/// Read the host's allowed-signers file as a container upload.
///
/// Returns `None` when SSH signing is not configured on the host or the
/// configured file cannot be read, in which case nothing is forwarded.
pub fn read_allowed_signers_upload(
    remote_user: &str,
    workspace_folder: &Path,
) -> Option<FileUpload> {
    let host_path = resolve_host_path(&RESOLVE_ARGS, workspace_folder)?;
    build_upload(&host_path, remote_user)
}

/// Copy the host's SSH signing key into the container when its value names a file.
///
/// Gated on `gpg.format` resolving to `ssh`: under `openpgp` the value is a GPG
/// key id and means nothing on the filesystem. A value git reads as literal key
/// material is forwarded unchanged; a value naming a file that cannot be read
/// is dropped, so the container never inherits a path that was never there.
pub fn read_signing_key(
    entries: &[(String, String)],
    remote_user: &str,
    workspace_folder: &Path,
) -> SigningKeyForwarding {
    let verbatim = SigningKeyForwarding {
        upload: None,
        value: SigningKeyValue::Verbatim,
    };

    if resolve_last(entries, GPG_FORMAT_KEY) != Some("ssh") {
        return verbatim;
    }
    let Some(value) = resolve_last(entries, SIGNING_KEY) else {
        return verbatim;
    };
    if is_literal_key(value) {
        return verbatim;
    }

    let Some(host_path) = resolve_host_path(&SIGNING_KEY_ARGS, workspace_folder) else {
        return SigningKeyForwarding {
            upload: None,
            value: SigningKeyValue::Omitted,
        };
    };
    build_signing_key(&host_path, remote_user)
}

/// Build the container upload for a host signing key file.
///
/// The key is dropped rather than forwarded when its file cannot be read, so
/// the container never inherits a path that was never there.
fn build_signing_key(host_path: &Path, remote_user: &str) -> SigningKeyForwarding {
    let Some(content) = read_public_key(host_path) else {
        return SigningKeyForwarding {
            upload: None,
            value: SigningKeyValue::Omitted,
        };
    };

    let container_path = format!("{}/{SIGNING_KEY_FILENAME}", remote_ssh_dir(remote_user));
    SigningKeyForwarding {
        upload: Some(FileUpload {
            container_path: container_path.clone(),
            content,
            mode: 0o600,
        }),
        value: SigningKeyValue::Rewritten(container_path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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
    fn literal_key_material_is_recognized() {
        // git's `is_literal_ssh_key`: a `key::` prefix, or the deprecated bare `ssh-` spelling.
        assert!(is_literal_key(
            "key::ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE me@example.com"
        ));
        assert!(is_literal_key("key::ecdsa-sha2-nistp256 AAAAE2Vj"));
        assert!(is_literal_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE me@example.com"
        ));
        assert!(is_literal_key("ssh-rsa AAAAB3NzaC1yc2E"));
    }

    #[test]
    fn key_paths_are_not_mistaken_for_key_material() {
        assert!(!is_literal_key("~/.ssh/id_ed25519.pub"));
        assert!(!is_literal_key("/Users/me/.ssh/id_ed25519.pub"));
        assert!(!is_literal_key("C:\\Users\\me\\.ssh\\id_ed25519.pub"));
        // An openpgp key id, which never reaches the classifier but must not classify as key material if it did.
        assert!(!is_literal_key("ABCD1234"));
        // git's checks are byte-exact, so these near-misses are filenames.
        assert!(!is_literal_key("KEY::ssh-ed25519 AAAA"));
        assert!(!is_literal_key("SSH-ed25519 AAAA"));
        assert!(!is_literal_key("~/.ssh/ssh-key.pub"));
    }

    #[test]
    fn private_key_without_a_public_sibling_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let private = tmp.path().join("id_ed25519");
        fs::write(
            &private,
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1r\n-----END OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();

        assert!(
            read_public_key(&private).is_none(),
            "a private key with no public counterpart must not be forwarded"
        );
    }

    #[test]
    fn signing_key_is_verbatim_when_the_format_is_not_ssh() {
        let tmp = TempDir::new().unwrap();
        let entries = vec![
            ("gpg.format".to_string(), "openpgp".to_string()),
            ("user.signingkey".to_string(), "ABCD1234".to_string()),
        ];

        let forwarding = read_signing_key(&entries, "vscode", tmp.path());
        assert!(forwarding.upload.is_none());
        assert_eq!(
            forwarding.value,
            SigningKeyValue::Verbatim,
            "a gpg key id is not a path and must be left alone"
        );
    }

    #[test]
    fn signing_key_is_verbatim_when_the_value_is_key_material() {
        let tmp = TempDir::new().unwrap();
        let entries = vec![
            ("gpg.format".to_string(), "ssh".to_string()),
            (
                "user.signingkey".to_string(),
                "key::ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE".to_string(),
            ),
        ];

        let forwarding = read_signing_key(&entries, "vscode", tmp.path());
        assert!(forwarding.upload.is_none());
        assert_eq!(forwarding.value, SigningKeyValue::Verbatim);
    }

    #[test]
    fn signing_key_gate_follows_the_last_gpg_format() {
        let tmp = TempDir::new().unwrap();
        // An include switches the host back to gpg: the value is a key id again.
        let entries = vec![
            ("gpg.format".to_string(), "ssh".to_string()),
            (
                "user.signingkey".to_string(),
                "~/.ssh/id_ed25519.pub".to_string(),
            ),
            ("gpg.format".to_string(), "openpgp".to_string()),
            ("user.signingkey".to_string(), "ABCD1234".to_string()),
        ];

        let forwarding = read_signing_key(&entries, "vscode", tmp.path());
        assert_eq!(forwarding.value, SigningKeyValue::Verbatim);
    }

    #[test]
    fn signing_key_path_is_copied_and_rewritten() {
        let tmp = TempDir::new().unwrap();
        let key = tmp.path().join("id_ed25519.pub");
        let content = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE me@example.com\n";
        fs::write(&key, content).unwrap();

        let forwarding = build_signing_key(&key, "vscode");
        let upload = forwarding.upload.expect("the public key should be copied");
        assert_eq!(upload.container_path, "/home/vscode/.ssh/signing_key.pub");
        assert_eq!(upload.content, content.as_bytes());
        assert_eq!(upload.mode, 0o600);
        assert_eq!(
            forwarding.value,
            SigningKeyValue::Rewritten("/home/vscode/.ssh/signing_key.pub".to_string()),
            "the forwarded value must name the copy, not the host path"
        );
    }

    #[test]
    fn signing_key_private_half_is_swapped_for_the_public_one() {
        let tmp = TempDir::new().unwrap();
        let private = tmp.path().join("id_ed25519");
        fs::write(
            &private,
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1r\n-----END OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();
        let public = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE me@example.com\n";
        fs::write(tmp.path().join("id_ed25519.pub"), public).unwrap();

        let upload = build_signing_key(&private, "vscode")
            .upload
            .expect("the public half should be copied");
        assert_eq!(upload.content, public.as_bytes());
    }

    #[test]
    fn signing_key_missing_file_is_omitted() {
        let tmp = TempDir::new().unwrap();

        let forwarding = build_signing_key(&tmp.path().join("absent.pub"), "vscode");
        assert!(forwarding.upload.is_none());
        assert_eq!(
            forwarding.value,
            SigningKeyValue::Omitted,
            "a signing key that is not on disk must not be forwarded"
        );
    }

    #[test]
    fn signing_key_empty_file_is_omitted() {
        let tmp = TempDir::new().unwrap();
        let key = tmp.path().join("id_ed25519.pub");
        fs::write(&key, "").unwrap();

        assert_eq!(
            build_signing_key(&key, "vscode").value,
            SigningKeyValue::Omitted
        );
    }

    #[test]
    fn a_relative_key_path_is_anchored_to_the_workspace_folder() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(workspace.join("keys")).unwrap();
        let content = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE me@example.com\n";
        fs::write(workspace.join("keys/signing.pub"), content).unwrap();

        let global = tmp.path().join("global");
        fs::write(&global, "[user]\n\tsigningkey = keys/signing.pub\n").unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = std::process::Command::new("git")
            .args(SIGNING_KEY_ARGS)
            .current_dir(&workspace)
            .env("GIT_CONFIG_GLOBAL", &global)
            .output()
        else {
            return;
        };
        assert!(output.status.success(), "git config --get should succeed");
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            raw, "keys/signing.pub",
            "git leaves a relative value relative"
        );

        // The anchoring `resolve_host_path` applies, which is what makes the read find the file.
        let anchored = workspace.join(&raw);
        assert_eq!(
            build_signing_key(&anchored, "vscode")
                .upload
                .expect("the key should be found under the workspace folder")
                .content,
            content.as_bytes()
        );
    }

    #[test]
    fn signing_key_args_expand_tilde_and_follow_includes() {
        let tmp = TempDir::new().unwrap();
        let included = tmp.path().join("included");
        fs::write(&included, "[user]\n\tsigningkey = ~/.ssh/id_ed25519.pub\n").unwrap();
        let global = tmp.path().join("global");
        fs::write(
            &global,
            format!("[include]\n\tpath = {}\n", included.display()),
        )
        .unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = std::process::Command::new("git")
            .args(SIGNING_KEY_ARGS)
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
            tmp.path().join(".ssh/id_ed25519.pub").display().to_string(),
            "the resolved path must follow includes and expand ~"
        );
    }

    #[test]
    fn signing_key_container_path_follows_the_remote_user() {
        assert_eq!(
            format!("{}/{SIGNING_KEY_FILENAME}", remote_ssh_dir("root")),
            "/root/.ssh/signing_key.pub"
        );
        assert_eq!(
            format!("{}/{SIGNING_KEY_FILENAME}", remote_ssh_dir("node")),
            "/home/node/.ssh/signing_key.pub"
        );
    }

    #[test]
    fn signing_key_filename_collides_with_no_other_upload() {
        // Everything cella writes into the container's `.ssh` directory.
        let written = ["known_hosts", "config", ALLOWED_SIGNERS_FILENAME];
        assert!(
            !written.contains(&SIGNING_KEY_FILENAME),
            "the signing key must not overwrite another forwarded file"
        );
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
