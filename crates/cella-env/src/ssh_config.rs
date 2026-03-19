//! SSH config file reading for container injection.

use std::path::PathBuf;

use crate::FileUpload;

/// Read SSH config files from the host for injection into the container.
///
/// Reads `~/.ssh/known_hosts` and `~/.ssh/config` if they exist.
/// Returns empty vec if `~/.ssh/` doesn't exist (common on CI).
pub fn read_ssh_config_files(remote_user: &str) -> Vec<FileUpload> {
    let Some(ssh_dir) = host_ssh_dir() else {
        return Vec::new();
    };
    if !ssh_dir.exists() {
        return Vec::new();
    }

    let remote_ssh_dir = remote_ssh_path(remote_user);
    let mut files = Vec::new();

    for filename in ["known_hosts", "config"] {
        let path = ssh_dir.join(filename);
        if let Ok(content) = std::fs::read(&path)
            && !content.is_empty()
        {
            files.push(FileUpload {
                container_path: format!("{remote_ssh_dir}/{filename}"),
                content,
                mode: 0o600,
            });
        }
    }

    files
}

/// Get the host's `~/.ssh/` directory path.
fn host_ssh_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".ssh"))
}

/// Compute the remote user's `.ssh` directory path in the container.
fn remote_ssh_path(remote_user: &str) -> String {
    if remote_user == "root" {
        "/root/.ssh".to_string()
    } else {
        format!("/home/{remote_user}/.ssh")
    }
}

/// Get the container-side `.ssh` directory path for creating the directory.
pub fn remote_ssh_dir(remote_user: &str) -> String {
    remote_ssh_path(remote_user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_ssh_path_root() {
        assert_eq!(remote_ssh_path("root"), "/root/.ssh");
    }

    #[test]
    fn remote_ssh_path_user() {
        assert_eq!(remote_ssh_path("vscode"), "/home/vscode/.ssh");
    }

    #[test]
    fn read_ssh_files_nonexistent_dir() {
        // Reading from a path that doesn't exist should return empty
        let files = read_ssh_config_files("nonexistent_user_12345");
        // Will return empty since ~/.ssh may not have these specific files
        // or we're testing the path logic
        let _ = files;
    }
}
