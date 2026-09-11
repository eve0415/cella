//! Host git config parsing and safe subset filtering.

use std::path::Path;
use std::process::Output;

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

/// Exit status git uses for a fatal error.
///
/// `git config` reports on the config itself with small codes — 1 for a key
/// that is not set — and uses this one when it gave up before reading any
/// config at all. A directory git refuses to run in is one cause; a global
/// config file that is not there is another, which is why the retry below
/// decides between them rather than the code alone.
const GIT_FATAL: i32 = 128;

/// Run a host `git` invocation, optionally from a given directory.
fn run_host_git(args: &[&str], from: Option<&Path>) -> std::io::Result<Output> {
    let mut command = std::process::Command::new("git");
    command.args(args);
    if let Some(dir) = from {
        command.current_dir(dir);
    }
    command.output()
}

/// Run a host `git` invocation from the workspace folder.
///
/// `--global` still decides which files are read; the working directory only decides which `includeIf gitdir:` and `onbranch:` conditions match, so a user who narrows their signing config to one repository gets the config git would give them while standing in it.
///
/// Two cases fall back to running without a working directory, because losing `includeIf` matching costs one conditional value while failing the read costs every forwarded key. A folder that is not there cannot be a working directory at all — `Command::current_dir` would turn that into a spawn failure indistinguishable from git being missing. A folder git refuses to stand in, which a `.git` file naming a gitdir that no longer exists produces, fails every invocation with `fatal: not a git repository` before reading any config.
pub(crate) fn host_git_output(args: &[&str], workspace_folder: &Path) -> std::io::Result<Output> {
    if !workspace_folder.is_dir() {
        return run_host_git(args, None);
    }

    let output = run_host_git(args, Some(workspace_folder))?;
    if output.status.code() != Some(GIT_FATAL) {
        return Ok(output);
    }

    // Git gave up before reading any config. Retrying without the working directory says which cause it was: succeeding means the workspace folder was the problem, and failing the same way means it was not, so the original result stands.
    let fallback = run_host_git(args, None)?;
    if fallback.status.code() == Some(GIT_FATAL) {
        return Ok(output);
    }

    warn!(
        "Reading host git config from {} failed ({}), falling back to the unconditional global config",
        workspace_folder.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(fallback)
}

/// Read host git config and return the safe subset for container injection.
///
/// Invokes `git config --global --includes --list --null` on the host, from
/// `workspace_folder`, and filters through an allowlist of safe keys.
/// Returns empty vec if git is not installed or has no global config.
///
/// `allowed_signers_path` is the container-side path of the forwarded
/// allowed-signers file, or `None` when it could not be forwarded.
pub fn read_host_git_config(
    workspace_folder: &Path,
    allowed_signers_path: Option<&str>,
) -> Vec<GitConfigEntry> {
    let output = host_git_output(&LIST_ARGS, workspace_folder);

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

/// The key naming the file git verifies SSH signatures against.
pub(crate) const ALLOWED_SIGNERS_KEY: &str = "gpg.ssh.allowedSignersFile";

/// Resolve a key the way git does, to its last occurrence rather than its first.
///
/// A repeated key is routine once includes are followed: the global file and a
/// file it includes can each set one, and git takes the later value.
fn resolve_last<'a>(entries: &'a [(String, String)], wanted: &str) -> Option<&'a str> {
    entries
        .iter()
        .rev()
        .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.as_str())
}

/// Forward a key unless the allowlist already forwarded it.
///
/// The host's own value wins wherever it made it through the allowlist.
fn push_if_absent(safe: &mut Vec<GitConfigEntry>, key: &str, value: &str) {
    if safe.iter().any(|e| e.key.eq_ignore_ascii_case(key)) {
        return;
    }
    safe.push(GitConfigEntry {
        key: key.to_string(),
        value: value.to_string(),
    });
}

/// If SSH signing is configured, include related keys that aren't already present.
fn include_ssh_signing_keys(entries: &[(String, String)], safe: &mut Vec<GitConfigEntry>) {
    if resolve_last(entries, "gpg.format") != Some("ssh") {
        return;
    }

    for key in SSH_SIGNING_KEYS {
        if let Some(value) = resolve_last(entries, key) {
            push_if_absent(safe, key, value);
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
    if let Some(container_path) = allowed_signers_path {
        push_if_absent(safe, ALLOWED_SIGNERS_KEY, container_path);
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
    fn working_directory_selects_the_matching_conditional_include() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();

        let included = tmp.path().join("included");
        std::fs::write(&included, "[user]\n\tname = Workspace Name\n").unwrap();
        let global = tmp.path().join("global");
        std::fs::write(
            &global,
            format!(
                "[user]\n\tname = Global Name\n[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
                workspace.display(),
                included.display()
            ),
        )
        .unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(status) = std::process::Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(&workspace)
            .env("GIT_CONFIG_GLOBAL", &global)
            .status()
        else {
            return;
        };
        assert!(status.success(), "git init should succeed");

        // Mirrors `host_git_output`, with `GIT_CONFIG_GLOBAL` scoped to the child rather than set on this process.
        let name_in = |dir: &Path| {
            let output = std::process::Command::new("git")
                .args(LIST_ARGS)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", &global)
                .output()
                .expect("git was available a moment ago");
            let raw = String::from_utf8_lossy(&output.stdout).into_owned();
            resolve_last(&parse_null_delimited_config(&raw), "user.name")
                .unwrap_or_default()
                .to_string()
        };

        assert_eq!(
            name_in(&workspace),
            "Workspace Name",
            "a gitdir-conditional include must match when git runs in the workspace folder"
        );
        assert_eq!(
            name_in(&elsewhere),
            "Global Name",
            "a folder outside the condition must fall back to the unconditional value"
        );
    }

    #[test]
    fn a_workspace_folder_git_refuses_falls_back_to_the_unconditional_read() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global");
        std::fs::write(&global, "[user]\n\tname = Global Name\n").unwrap();

        // A stale worktree pointer: `.git` names a gitdir that is not there, and git refuses to run in the directory at all.
        let stale = tmp.path().join("stale");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join(".git"), "gitdir: /nonexistent/path\n").unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(refused) = std::process::Command::new("git")
            .args(LIST_ARGS)
            .current_dir(&stale)
            .env("GIT_CONFIG_GLOBAL", &global)
            .output()
        else {
            return;
        };
        assert_eq!(
            refused.status.code(),
            Some(GIT_FATAL),
            "the fixture must be a directory git actually refuses"
        );

        let recovered = std::process::Command::new("git")
            .args(LIST_ARGS)
            .env("GIT_CONFIG_GLOBAL", &global)
            .output()
            .expect("git was available a moment ago");
        let raw = String::from_utf8_lossy(&recovered.stdout).into_owned();
        assert_eq!(
            resolve_last(&parse_null_delimited_config(&raw), "user.name"),
            Some("Global Name"),
            "dropping the working directory must recover every forwarded key"
        );
    }

    #[test]
    fn an_unset_key_is_not_mistaken_for_a_refused_directory() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global");
        std::fs::write(&global, "[user]\n\tname = Global Name\n").unwrap();

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = std::process::Command::new("git")
            .args([
                "config",
                "--global",
                "--includes",
                "--get",
                "user.signingkey",
            ])
            .current_dir(tmp.path())
            .env("GIT_CONFIG_GLOBAL", &global)
            .output()
        else {
            return;
        };
        assert!(!output.status.success(), "the key is not set");
        assert_ne!(
            output.status.code(),
            Some(GIT_FATAL),
            "an unset key must not trigger the fallback, which would resolve it from cella's own directory"
        );
    }

    #[test]
    fn host_git_output_tolerates_a_missing_workspace_folder() {
        let tmp = TempDir::new().unwrap();
        let absent = tmp.path().join("gone");

        // Skips when git is unavailable, like the rest of the host-git tests.
        let Ok(output) = host_git_output(&["--version"], &absent) else {
            return;
        };
        assert!(
            output.status.success(),
            "a workspace folder that is not there must not turn into a spawn failure"
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
            .find(|e| e.key.eq_ignore_ascii_case(ALLOWED_SIGNERS_KEY))
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
            !safe
                .iter()
                .any(|e| e.key.eq_ignore_ascii_case(ALLOWED_SIGNERS_KEY)),
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
            .find(|e| e.key.eq_ignore_ascii_case(ALLOWED_SIGNERS_KEY))
            .expect("verification does not depend on the local signing format");
        assert_eq!(entry.value, "/home/node/.ssh/allowed_signers");
        assert!(
            !safe.iter().any(|e| e.key == "user.signingkey"),
            "a non-ssh signing format still forwards none of the gated keys"
        );
    }
}
