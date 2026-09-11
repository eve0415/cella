//! Host git config parsing and safe subset filtering.

use tracing::warn;

/// A git config key-value pair to inject into the container.
#[derive(Debug, Clone)]
pub struct GitConfigEntry {
    pub key: String,
    pub value: String,
}

/// Arguments used to list the host's global git config.
///
/// `--includes` is required because git turns include-following off by
/// default when a specific config file is selected with `--global`.
/// Without it, keys set in a file pulled in via `include.path` are invisible.
const LIST_ARGS: [&str; 5] = ["config", "--global", "--includes", "--list", "--null"];

/// Read host git config and return the safe subset for container injection.
///
/// Invokes `git config --global --includes --list --null` on the host and
/// filters through an allowlist of safe keys.
/// Returns empty vec if git is not installed or has no global config.
///
/// `allowed_signers_path` is the container-side path of the forwarded
/// allowed-signers file, or `None` when it could not be forwarded.
pub fn read_host_git_config(allowed_signers_path: Option<&str>) -> Vec<GitConfigEntry> {
    let output = std::process::Command::new("git").args(LIST_ARGS).output();

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
    include_allowed_signers_key(&mut safe, allowed_signers_path);

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

/// Git config keys forwarded when the host signs commits with SSH.
///
/// `gpg.ssh.allowedSignersFile` is deliberately absent: verification does not
/// depend on the signing format, so it is handled separately and ungated.
const SSH_SIGNING_KEYS: [&str; 4] = [
    "gpg.format",
    "user.signingkey",
    "commit.gpgsign",
    "tag.gpgsign",
];

/// Resolve a key the way git does, to its last occurrence rather than its first.
///
/// A repeated key is routine once includes are followed: the global file and a
/// file it includes can each set one, and git takes the later value.
fn resolve_last(entries: &[(String, String)], wanted: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.clone())
}

/// Check if a key points at the allowed-signers file.
const fn is_allowed_signers_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("gpg.ssh.allowedsignersfile")
}

/// If SSH signing is configured, include related keys that aren't already present.
fn include_ssh_signing_keys(entries: &[(String, String)], safe: &mut Vec<GitConfigEntry>) {
    if resolve_last(entries, "gpg.format").as_deref() != Some("ssh") {
        return;
    }

    for key in SSH_SIGNING_KEYS {
        if safe.iter().any(|e| e.key.eq_ignore_ascii_case(key)) {
            continue;
        }
        if let Some(value) = resolve_last(entries, key) {
            safe.push(GitConfigEntry {
                key: key.to_string(),
                value,
            });
        }
    }
}

/// Point the allowed-signers key at the file copied into the container.
///
/// Not gated on `gpg.format=ssh`: that selects the format commits are *signed*
/// with, while git verifies an SSH-signed commit through this file whichever
/// format the local user signs with. Gating it would leave someone who signs
/// with GPG unable to verify a colleague's SSH-signed commits.
///
/// The host value is never forwarded — it names a path that does not exist in
/// the container. When the file could not be copied the key is left out
/// entirely, so git reports it as unconfigured rather than chasing a path that
/// was never there.
fn include_allowed_signers_key(safe: &mut Vec<GitConfigEntry>, allowed_signers_path: Option<&str>) {
    let Some(container_path) = allowed_signers_path else {
        return;
    };
    if safe.iter().any(|e| is_allowed_signers_key(&e.key)) {
        return;
    }
    safe.push(GitConfigEntry {
        key: "gpg.ssh.allowedSignersFile".to_string(),
        value: container_path.to_string(),
    });
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
    use tempfile::TempDir;

    #[test]
    fn list_args_follow_include_directives() {
        let tmp = TempDir::new().unwrap();
        let included = tmp.path().join("included");
        std::fs::write(&included, "[user]\n\tname = Included Name\n").unwrap();
        let global = tmp.path().join("global");
        std::fs::write(
            &global,
            format!("[include]\n\tpath = {}\n", included.display()),
        )
        .unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = std::process::Command::new("git")
            .args(LIST_ARGS)
            .env("GIT_CONFIG_GLOBAL", &global)
            .output()
        else {
            return;
        };
        assert!(output.status.success(), "git config --list should succeed");

        let raw = String::from_utf8_lossy(&output.stdout);
        let safe = filter_safe_config(&parse_null_delimited_config(&raw));
        assert!(
            safe.iter()
                .any(|e| e.key == "user.name" && e.value == "Included Name"),
            "a key set in an included file must be forwarded"
        );
    }

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
        assert_eq!(
            SSH_SIGNING_KEYS,
            [
                "gpg.format",
                "user.signingkey",
                "commit.gpgsign",
                "tag.gpgsign"
            ]
        );
        assert!(
            !SSH_SIGNING_KEYS.contains(&"gpg.ssh.allowedSignersFile"),
            "the allowed-signers key is handled separately, not gated on gpg.format"
        );
    }

    #[test]
    fn signing_keys_resolve_to_the_last_occurrence() {
        // An include that switches the host to GPG signing: git reads the later
        // value, so nothing SSH-specific may reach the container.
        let raw = "gpg.format\nssh\0user.signingkey\n~/.ssh/stale.pub\0gpg.format\nopenpgp\0user.signingkey\nABCD1234\0";
        let entries = parse_null_delimited_config(raw);
        let mut safe = filter_safe_config(&entries);
        include_ssh_signing_keys(&entries, &mut safe);

        assert!(
            safe.is_empty(),
            "a host that no longer signs with SSH must forward no signing keys"
        );
    }

    #[test]
    fn signing_keys_take_the_last_value_when_repeated() {
        // The reverse: an include turns SSH signing on and names the real key.
        let raw = "gpg.format\nopenpgp\0user.signingkey\nABCD1234\0gpg.format\nssh\0user.signingkey\n~/.ssh/real.pub\0";
        let entries = parse_null_delimited_config(raw);
        let mut safe = filter_safe_config(&entries);
        include_ssh_signing_keys(&entries, &mut safe);

        let signing_key = safe
            .iter()
            .find(|e| e.key == "user.signingkey")
            .expect("signing key should be forwarded");
        assert_eq!(signing_key.value, "~/.ssh/real.pub");
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

    /// Null-delimited config with SSH signing and a host allowed-signers path.
    fn signing_entries() -> Vec<(String, String)> {
        let raw = "gpg.format\nssh\0user.signingkey\n~/.ssh/id_ed25519.pub\0commit.gpgsign\ntrue\0tag.gpgsign\ntrue\0gpg.ssh.allowedsignersfile\n/Users/me/.config/git/allowed_signers\0";
        parse_null_delimited_config(raw)
    }

    #[test]
    fn allowed_signers_rewritten_to_container_path() {
        let entries = signing_entries();
        let mut safe = filter_safe_config(&entries);
        include_allowed_signers_key(&mut safe, Some("/home/node/.ssh/allowed_signers"));

        let entry = safe
            .iter()
            .find(|e| is_allowed_signers_key(&e.key))
            .expect("allowed signers key should be forwarded");
        assert_eq!(entry.value, "/home/node/.ssh/allowed_signers");
    }

    #[test]
    fn other_signing_keys_stay_verbatim() {
        let entries = signing_entries();
        let mut safe = filter_safe_config(&entries);
        include_ssh_signing_keys(&entries, &mut safe);

        let signing_key = safe
            .iter()
            .find(|e| e.key == "user.signingkey")
            .expect("signing key should be forwarded");
        assert_eq!(signing_key.value, "~/.ssh/id_ed25519.pub");
        assert!(safe.iter().any(|e| e.key == "tag.gpgsign"));
    }

    #[test]
    fn allowed_signers_omitted_when_not_forwarded() {
        let entries = signing_entries();
        let mut safe = filter_safe_config(&entries);
        include_ssh_signing_keys(&entries, &mut safe);
        include_allowed_signers_key(&mut safe, None);

        assert!(
            !safe.iter().any(|e| is_allowed_signers_key(&e.key)),
            "an unforwardable file must not leave a dangling host path behind"
        );
        assert!(
            safe.iter().any(|e| e.key == "gpg.format"),
            "the remaining signing keys are still forwarded"
        );
    }

    #[test]
    fn allowed_signers_forwarded_when_signing_format_is_not_ssh() {
        let raw = "gpg.format\nopenpgp\0user.signingkey\nABCD1234\0";
        let entries = parse_null_delimited_config(raw);
        let mut safe = filter_safe_config(&entries);
        include_ssh_signing_keys(&entries, &mut safe);
        include_allowed_signers_key(&mut safe, Some("/home/node/.ssh/allowed_signers"));

        let entry = safe
            .iter()
            .find(|e| is_allowed_signers_key(&e.key))
            .expect("verification does not depend on the local signing format");
        assert_eq!(entry.value, "/home/node/.ssh/allowed_signers");
        assert!(
            !safe.iter().any(|e| e.key == "user.signingkey"),
            "a non-ssh signing format still forwards none of the gated keys"
        );
    }

    #[test]
    fn allowed_signers_key_matched_case_insensitively() {
        assert!(is_allowed_signers_key("gpg.ssh.allowedSignersFile"));
        assert!(is_allowed_signers_key("gpg.ssh.allowedsignersfile"));
        assert!(!is_allowed_signers_key("gpg.ssh.defaultKeyCommand"));
    }
}
