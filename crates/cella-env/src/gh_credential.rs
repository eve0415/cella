//! GitHub CLI credential detection and file generation for container injection.
//!
//! Detects host gh CLI authentication, extracts tokens, and generates
//! `hosts.yml` and `config.yml` files for seeding into containers.

use std::path::Path;
use std::process::Command;

use crate::FileUpload;

/// Result of preparing gh CLI credential files for container injection.
pub struct GhCredentialForwarding {
    /// Files to upload into the container (`hosts.yml` and optionally `config.yml`).
    pub file_uploads: Vec<FileUpload>,
}

/// Detect and prepare gh CLI credential files for container injection.
///
/// Returns `None` if gh CLI is not installed or not authenticated.
///
/// The detection flow:
/// 1. Check `gh auth status` (exit 0 = authenticated)
/// 2. Parse workspace git remotes for GitHub hostnames
/// 3. Extract tokens via `gh auth token -h <hostname>`
/// 4. Build `hosts.yml` content
/// 5. Read host's `~/.config/gh/config.yml` if it exists
pub fn prepare_gh_credentials(
    workspace_root: &Path,
    remote_user: &str,
) -> Option<GhCredentialForwarding> {
    // Check if gh is installed and authenticated
    if !gh_is_authenticated() {
        tracing::debug!(
            "gh CLI not installed or not authenticated, skipping credential forwarding"
        );
        return None;
    }

    // Determine GitHub hostnames from git remotes
    let hostnames = github_hostnames_from_remotes(workspace_root);
    let hostnames = if hostnames.is_empty() {
        vec!["github.com".to_string()]
    } else {
        hostnames
    };

    // Extract tokens and build hosts.yml
    let hosts_yml = build_hosts_yml(&hostnames)?;

    let config_dir = gh_config_dir_for_user(remote_user);

    let mut uploads = vec![FileUpload {
        container_path: format!("{config_dir}/hosts.yml"),
        content: hosts_yml.into_bytes(),
        mode: 0o600,
    }];

    // Copy host's config.yml if it exists
    if let Some(config_yml) = read_host_gh_config() {
        uploads.push(FileUpload {
            container_path: format!("{config_dir}/config.yml"),
            content: config_yml.into_bytes(),
            mode: 0o600,
        });
    }

    Some(GhCredentialForwarding {
        file_uploads: uploads,
    })
}

/// Container-side gh config directory for a given user.
pub fn gh_config_dir_for_user(remote_user: &str) -> String {
    if remote_user == "root" {
        "/root/.config/gh".to_string()
    } else {
        format!("/home/{remote_user}/.config/gh")
    }
}

/// Check if `gh auth status` succeeds (exit code 0).
pub fn gh_is_authenticated() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Host-side GitHub CLI status.
pub struct HostGhStatus {
    /// Whether `gh` is installed.
    pub installed: bool,
    /// Whether `gh auth status` succeeds.
    pub authenticated: bool,
    /// stderr output from `gh auth status` when authenticated.
    pub status_output: Option<String>,
}

/// Probe the host's GitHub CLI installation and auth status.
pub fn probe_host_gh_status() -> HostGhStatus {
    let output = Command::new("gh").args(["auth", "status"]).output();
    match output {
        Ok(o) if o.status.success() => HostGhStatus {
            installed: true,
            authenticated: true,
            status_output: Some(String::from_utf8_lossy(&o.stderr).to_string()),
        },
        Ok(_) => HostGhStatus {
            installed: true,
            authenticated: false,
            status_output: None,
        },
        Err(_) => HostGhStatus {
            installed: false,
            authenticated: false,
            status_output: None,
        },
    }
}

/// Extract GitHub hostnames from workspace git remotes.
///
/// Parses `.git/config` for remote URLs matching `github.com` or `*.github.com`.
fn github_hostnames_from_remotes(workspace_root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(workspace_root)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut hostnames: Vec<String> = Vec::new();

    for line in stdout.lines() {
        if let Some(hostname) = extract_github_hostname(line)
            && !hostnames.contains(&hostname)
        {
            hostnames.push(hostname);
        }
    }

    hostnames
}

/// Extract a GitHub hostname from a git remote line.
///
/// Handles SSH (`git@github.com:user/repo.git`) and
/// HTTPS (`https://github.com/user/repo.git`) URL formats.
fn extract_github_hostname(remote_line: &str) -> Option<String> {
    // Split on whitespace: name, url, (fetch)/(push)
    let url = remote_line.split_whitespace().nth(1)?;

    // SSH format: git@github.com:user/repo.git
    if let Some(rest) = url.strip_prefix("git@") {
        let hostname = rest.split(':').next()?;
        if is_github_hostname(hostname) {
            return Some(hostname.to_string());
        }
    }

    // HTTPS format: https://github.com/user/repo.git
    if url.starts_with("https://") || url.starts_with("http://") {
        let without_scheme = url.split("://").nth(1)?;
        let hostname = without_scheme.split('/').next()?;
        if is_github_hostname(hostname) {
            return Some(hostname.to_string());
        }
    }

    None
}

/// Check if a hostname is GitHub (github.com or *.github.com for GHES).
fn is_github_hostname(hostname: &str) -> bool {
    hostname == "github.com" || hostname.ends_with(".github.com")
}

/// Build `hosts.yml` content by extracting tokens for each hostname.
fn build_hosts_yml(hostnames: &[String]) -> Option<String> {
    use std::fmt::Write;

    let mut yaml = String::new();
    let mut any_token = false;

    for hostname in hostnames {
        if let Some(token_info) = extract_token_info(hostname) {
            let _ = writeln!(yaml, "{hostname}:");
            let _ = writeln!(yaml, "    oauth_token: {}", token_info.token);
            if let Some(ref user) = token_info.user {
                let _ = writeln!(yaml, "    user: {user}");
            }
            let _ = writeln!(
                yaml,
                "    git_protocol: {}",
                token_info.git_protocol.as_deref().unwrap_or("https")
            );
            any_token = true;
        }
    }

    if any_token { Some(yaml) } else { None }
}

struct TokenInfo {
    token: String,
    user: Option<String>,
    git_protocol: Option<String>,
}

/// Extract token, username, and git protocol for a hostname via gh CLI.
fn extract_token_info(hostname: &str) -> Option<TokenInfo> {
    let token = Command::new("gh")
        .args(["auth", "token", "-h", hostname])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;

    if token.is_empty() {
        return None;
    }

    // Get username via gh api
    let user = Command::new("gh")
        .args([
            "api",
            "-H",
            &format!("Host: {hostname}"),
            "/user",
            "--jq",
            ".login",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|u| !u.is_empty());

    // Get git protocol preference
    let git_protocol = Command::new("gh")
        .args(["config", "get", "git_protocol", "-h", hostname])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty());

    Some(TokenInfo {
        token,
        user,
        git_protocol,
    })
}

/// Read the host's `~/.config/gh/config.yml` file.
fn read_host_gh_config() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home)
        .join(".config")
        .join("gh")
        .join("config.yml");
    std::fs::read_to_string(path).ok()
}

/// Check if gh credential files exist in a container at the given config dir.
pub fn gh_config_exists_in_container(config_dir: &str) -> Vec<String> {
    // Returns the commands to check for gh config existence
    vec![
        "test".to_string(),
        "-f".to_string(),
        format!("{config_dir}/hosts.yml"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_github_hostname_ssh() {
        let line = "origin\tgit@github.com:user/repo.git (fetch)";
        assert_eq!(
            extract_github_hostname(line),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn extract_github_hostname_https() {
        let line = "origin\thttps://github.com/user/repo.git (fetch)";
        assert_eq!(
            extract_github_hostname(line),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn extract_github_hostname_ghes() {
        let line = "origin\tgit@git.enterprise.github.com:org/repo.git (fetch)";
        assert_eq!(
            extract_github_hostname(line),
            Some("git.enterprise.github.com".to_string())
        );
    }

    #[test]
    fn extract_github_hostname_non_github() {
        let line = "origin\tgit@gitlab.com:user/repo.git (fetch)";
        assert_eq!(extract_github_hostname(line), None);
    }

    #[test]
    fn extract_github_hostname_bitbucket() {
        let line = "origin\thttps://bitbucket.org/user/repo.git (fetch)";
        assert_eq!(extract_github_hostname(line), None);
    }

    #[test]
    fn is_github_hostname_tests() {
        assert!(is_github_hostname("github.com"));
        assert!(is_github_hostname("git.enterprise.github.com"));
        assert!(!is_github_hostname("gitlab.com"));
        assert!(!is_github_hostname("notgithub.com"));
    }

    #[test]
    fn build_hosts_yml_format() {
        // We can't test with real gh CLI, but we can test the format builder
        // by checking the function's structure
        let hostnames = vec!["github.com".to_string()];
        // This will return None since gh CLI isn't available in test env
        let result = build_hosts_yml(&hostnames);
        // In CI/test environment, gh likely isn't authenticated
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn gh_config_dir_root() {
        assert_eq!(gh_config_dir_for_user("root"), "/root/.config/gh");
    }

    #[test]
    fn gh_config_dir_regular_user() {
        assert_eq!(gh_config_dir_for_user("vscode"), "/home/vscode/.config/gh");
    }

    #[test]
    fn gh_config_exists_check_command() {
        let cmd = gh_config_exists_in_container("/home/vscode/.config/gh");
        assert_eq!(cmd, vec!["test", "-f", "/home/vscode/.config/gh/hosts.yml"]);
    }

    #[test]
    fn extract_multiple_remotes() {
        let workspace = std::env::temp_dir();
        // Smoke test — just ensure it doesn't panic on a non-git dir
        let hostnames = github_hostnames_from_remotes(&workspace);
        let _ = hostnames;
    }
}
