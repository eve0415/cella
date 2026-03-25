//! Host git config parsing and safe subset filtering.

use tracing::warn;

/// A git config key-value pair to inject into the container.
#[derive(Debug, Clone)]
pub struct GitConfigEntry {
    pub key: String,
    pub value: String,
}

/// Read host git config and return the safe subset for container injection.
///
/// Invokes `git config --global --list --null` on the host and filters
/// through an allowlist of safe keys. Returns empty vec if git is not
/// installed or has no global config.
pub fn read_host_git_config() -> Vec<GitConfigEntry> {
    let output = std::process::Command::new("git")
        .args(["config", "--global", "--list", "--null"])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Vec::new(), // No global config or git config error
        Err(_) => {
            warn!("git not found on host, skipping git config forwarding");
            return Vec::new();
        }
    };

    let raw = String::from_utf8_lossy(&output.stdout);
    let entries = parse_null_delimited_config(&raw);
    let mut safe = filter_safe_config(&entries);

    include_ssh_signing_keys(&entries, &mut safe);

    safe
}

/// Parse null-delimited git config output into key-value pairs.
///
/// Format: "key\nvalue\0key\nvalue\0..."
fn parse_null_delimited_config(raw: &str) -> Vec<(String, String)> {
    raw.split('\0')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (key, value) = entry.split_once('\n')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Filter git config entries through the safe allowlist.
fn filter_safe_config(entries: &[(String, String)]) -> Vec<GitConfigEntry> {
    entries
        .iter()
        .filter(|(key, _)| is_safe_key(key))
        .map(|(key, value)| GitConfigEntry {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// Check if a git config key is in the safe allowlist.
fn is_safe_key(key: &str) -> bool {
    let key_lower = key.to_lowercase();

    // Exact match allowlist
    let exact = [
        "user.name",
        "user.email",
        "core.autocrlf",
        "core.editor",
        "core.eol",
        "core.filemode",
        "core.ignorecase",
        "core.pager",
        "core.whitespace",
        "init.defaultbranch",
        "push.default",
        "push.autosetupremote",
        "pull.rebase",
        "pull.ff",
        "merge.ff",
        "diff.tool",
        "diff.algorithm",
        "merge.tool",
        "rebase.autosquash",
        "rerere.enabled",
        "fetch.prune",
        "commit.verbose",
    ];

    if exact.iter().any(|e| key_lower == *e) {
        return true;
    }

    // Prefix allowlist (all keys under these prefixes)
    let prefixes = ["alias.", "color."];

    if prefixes.iter().any(|p| key_lower.starts_with(p)) {
        return true;
    }

    false
}

/// Check if a key is an SSH signing config key.
fn is_ssh_signing_key(key: &str) -> bool {
    let key_lower = key.to_lowercase();
    matches!(
        key_lower.as_str(),
        "gpg.format"
            | "user.signingkey"
            | "commit.gpgsign"
            | "tag.gpgsign"
            | "gpg.ssh.allowedsignersfile"
    )
}

/// If SSH signing is configured, include related keys that aren't already present.
fn include_ssh_signing_keys(entries: &[(String, String)], safe: &mut Vec<GitConfigEntry>) {
    let has_ssh_signing = entries.iter().any(|(k, v)| k == "gpg.format" && v == "ssh");
    if has_ssh_signing {
        for (key, value) in entries {
            if is_ssh_signing_key(key) && !safe.iter().any(|e| e.key == *key) {
                safe.push(GitConfigEntry {
                    key: key.clone(),
                    value: value.clone(),
                });
            }
        }
    }
}

/// Check if a git config key is in the blocklist (never copy).
///
/// Used for documentation/testing — the allowlist approach means
/// anything not in the allowlist is already blocked.
#[cfg(test)]
fn is_blocked_key(key: &str) -> bool {
    let key_lower = key.to_lowercase();
    let prefixes = [
        "credential.",
        "gpg.",
        "core.sshcommand",
        "core.hookspath",
        "includeif.",
        "include.",
        "safe.directory",
        "http.",
        "url.",
        "remote.",
        "branch.",
    ];
    // gpg.format and gpg.ssh.* can be allowed for SSH signing,
    // but by default gpg.* is blocked
    prefixes.iter().any(|p| key_lower.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_keys_allowed() {
        assert!(is_safe_key("user.name"));
        assert!(is_safe_key("user.email"));
        assert!(is_safe_key("core.autocrlf"));
        assert!(is_safe_key("init.defaultBranch"));
        assert!(is_safe_key("push.default"));
        assert!(is_safe_key("pull.rebase"));
        assert!(is_safe_key("alias.co"));
        assert!(is_safe_key("alias.anything"));
        assert!(is_safe_key("color.ui"));
        assert!(is_safe_key("color.diff.meta"));
    }

    #[test]
    fn blocked_keys_rejected() {
        assert!(!is_safe_key("credential.helper"));
        assert!(!is_safe_key("core.sshcommand"));
        assert!(!is_safe_key("http.proxy"));
        assert!(!is_safe_key("url.ssh://git@github.com/.insteadof"));
        assert!(!is_safe_key("remote.origin.url"));
        assert!(!is_safe_key("branch.main.remote"));
        assert!(!is_safe_key("safe.directory"));
    }

    #[test]
    fn blocked_keys_match_function() {
        // Verify the blocklist function agrees with is_safe_key
        let blocked = [
            "credential.helper",
            "gpg.program",
            "core.sshcommand",
            "http.proxy",
            "url.ssh://git@github.com/.insteadof",
            "remote.origin.url",
            "branch.main.remote",
            "include.path",
            "includeif.onbranch:main.path",
            "safe.directory",
        ];
        for key in blocked {
            assert!(is_blocked_key(key), "{key} should be blocked");
            assert!(!is_safe_key(key), "{key} should not be safe");
        }
    }

    #[test]
    fn ssh_signing_keys_detected() {
        assert!(is_ssh_signing_key("gpg.format"));
        assert!(is_ssh_signing_key("user.signingkey"));
        assert!(is_ssh_signing_key("commit.gpgsign"));
        assert!(is_ssh_signing_key("tag.gpgsign"));
        assert!(is_ssh_signing_key("gpg.ssh.allowedSignersFile"));
    }

    #[test]
    fn parse_null_delimited() {
        let raw = "user.name\nJohn Doe\0user.email\njohn@example.com\0";
        let entries = parse_null_delimited_config(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            ("user.name".to_string(), "John Doe".to_string())
        );
        assert_eq!(
            entries[1],
            ("user.email".to_string(), "john@example.com".to_string())
        );
    }

    #[test]
    fn parse_empty_input() {
        let entries = parse_null_delimited_config("");
        assert!(entries.is_empty());
    }

    #[test]
    fn filter_safe_subset() {
        let entries = vec![
            ("user.name".to_string(), "John".to_string()),
            ("user.email".to_string(), "john@test.com".to_string()),
            ("credential.helper".to_string(), "store".to_string()),
            ("alias.co".to_string(), "checkout".to_string()),
            (
                "remote.origin.url".to_string(),
                "https://github.com/x".to_string(),
            ),
        ];
        let safe = filter_safe_config(&entries);
        assert_eq!(safe.len(), 3);
        assert!(safe.iter().any(|e| e.key == "user.name"));
        assert!(safe.iter().any(|e| e.key == "user.email"));
        assert!(safe.iter().any(|e| e.key == "alias.co"));
    }

    #[test]
    fn ssh_signing_config_included_when_detected() {
        let raw = "gpg.format\nssh\0user.signingkey\n~/.ssh/id_ed25519.pub\0commit.gpgsign\ntrue\0credential.helper\nstore\0";
        let entries = parse_null_delimited_config(raw);
        let mut safe = filter_safe_config(&entries);
        include_ssh_signing_keys(&entries, &mut safe);

        assert!(safe.iter().any(|e| e.key == "gpg.format"));
        assert!(safe.iter().any(|e| e.key == "user.signingkey"));
        assert!(safe.iter().any(|e| e.key == "commit.gpgsign"));
        assert!(!safe.iter().any(|e| e.key == "credential.helper"));
    }
}
