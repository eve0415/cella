//! Hostname parsing and DNS label sanitization.

use sha2::{Digest, Sha256};

const DNS_LABEL_MAX: usize = 63;

/// Sanitize a git branch name into a valid DNS label.
///
/// Rules applied in order:
/// 1. Lowercase
/// 2. Replace `/`, `_`, `.` with `-`
/// 3. Remove characters that aren't alphanumeric or `-`
/// 4. Collapse consecutive hyphens
/// 5. Strip leading/trailing hyphens
/// 6. Truncate to 63 characters (DNS label limit)
pub fn sanitize_branch(branch: &str) -> String {
    let replaced: String = branch
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            '/' | '_' | '.' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c,
            _ => '-',
        })
        .collect();

    let collapsed = collapse_hyphens(&replaced);
    let trimmed = collapsed.trim_matches('-');

    if trimmed.len() > DNS_LABEL_MAX {
        let truncated = &trimmed[..DNS_LABEL_MAX];
        truncated.trim_end_matches('-').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Produce a collision-safe slug by appending a 4-char hash suffix when
/// two different branch names sanitize to the same label.
///
/// Callers should check for collisions and call this only when needed.
pub fn sanitize_branch_with_suffix(branch: &str) -> String {
    let hash = hex::encode(Sha256::digest(branch.as_bytes()));
    let suffix = &hash[..4];
    let base = sanitize_branch(branch);

    let max_base = DNS_LABEL_MAX - 5; // "-" + 4-char hash
    let trimmed = if base.len() > max_base {
        base[..max_base].trim_end_matches('-').to_string()
    } else {
        base
    };

    format!("{trimmed}-{suffix}")
}

/// Parsed components from a `*.localhost` or `*.local` hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHostname {
    /// Container port (`None` for bare hostnames → use default port).
    pub port: Option<u16>,
    /// Sanitized branch slug.
    pub branch: String,
    /// Project slug.
    pub project: String,
}

/// Which TLD family the hostname belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostnameTld {
    Localhost,
    Local,
}

/// Parse a `Host` header value into routing components.
///
/// Expected formats:
/// - `{port}.{branch}.{project}.localhost` → port + branch + project
/// - `{branch}.{project}.localhost`        → bare (default port)
/// - Same patterns with `.local` (`OrbStack`)
///
/// Strips any `:port` suffix from the Host header before parsing.
pub fn parse_hostname(host: &str) -> Option<ParsedHostname> {
    // Strip port suffix (e.g., "foo.localhost:80" → "foo.localhost")
    let hostname = host.split(':').next().unwrap_or(host);
    let (labels, _tld) = split_tld(hostname)?;

    match labels.len() {
        // {branch}.{project}.{tld}
        2 => Some(ParsedHostname {
            port: None,
            branch: labels[0].to_string(),
            project: labels[1].to_string(),
        }),
        // {port}.{branch}.{project}.{tld}
        3 => {
            let port: u16 = labels[0].parse().ok()?;
            Some(ParsedHostname {
                port: Some(port),
                branch: labels[1].to_string(),
                project: labels[2].to_string(),
            })
        }
        _ => None,
    }
}

/// Split a hostname into (labels-before-tld, tld).
/// Returns `None` if the TLD is not `.localhost` or `.local`.
fn split_tld(hostname: &str) -> Option<(Vec<&str>, HostnameTld)> {
    let parts: Vec<&str> = hostname.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let last = *parts.last()?;
    if last.eq_ignore_ascii_case("localhost") {
        Some((parts[..parts.len() - 1].to_vec(), HostnameTld::Localhost))
    } else if parts.len() >= 3 && parts[parts.len() - 1].eq_ignore_ascii_case("local") {
        // For .local we don't want to confuse "something.local" (2 parts)
        // with our routing hostnames which always have 3+ parts before .local
        Some((parts[..parts.len() - 1].to_vec(), HostnameTld::Local))
    } else {
        None
    }
}

/// Build a hostname URL for a forwarded port, including the hostname proxy
/// listener port when it is not the default HTTP port.
///
/// Returns `None` when `proxy_port` is `None`, since hostname URLs require the
/// proxy to be running. `OrbStack`'s `.local` default-port domain is exposed via
/// Docker labels, not through this function.
pub fn build_hostname_url(
    port: u16,
    branch: &str,
    project: &str,
    proxy_port: Option<u16>,
) -> Option<String> {
    proxy_port?;
    let sanitized = sanitize_branch(branch);
    if matches!(proxy_port, Some(p) if p != 80) {
        Some(format!(
            "http://{port}.{sanitized}.{project}.localhost:{}",
            proxy_port.unwrap_or(80)
        ))
    } else {
        Some(format!("http://{port}.{sanitized}.{project}.localhost"))
    }
}

/// Build the bare hostname (without port) for default-port access.
pub fn build_bare_hostname(branch: &str, project: &str, is_orbstack: bool) -> String {
    let sanitized = sanitize_branch(branch);
    if is_orbstack {
        format!("{sanitized}.{project}.local")
    } else {
        format!("{sanitized}.{project}.localhost")
    }
}

fn collapse_hyphens(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slashes_become_hyphens() {
        assert_eq!(sanitize_branch("feature/auth-v2"), "feature-auth-v2");
    }

    #[test]
    fn nested_slashes() {
        assert_eq!(
            sanitize_branch("user/alice/experiment"),
            "user-alice-experiment"
        );
    }

    #[test]
    fn underscores_and_dots() {
        assert_eq!(sanitize_branch("bugfix_login.flow"), "bugfix-login-flow");
    }

    #[test]
    fn consecutive_hyphens_collapse() {
        assert_eq!(sanitize_branch("a---b"), "a-b");
    }

    #[test]
    fn mixed_separators_collapse() {
        assert_eq!(sanitize_branch("a/._b"), "a-b");
    }

    #[test]
    fn leading_trailing_hyphens_stripped() {
        assert_eq!(sanitize_branch("-abc-"), "abc");
        assert_eq!(sanitize_branch("/abc/"), "abc");
    }

    #[test]
    fn lowercased() {
        assert_eq!(sanitize_branch("Feature/Auth"), "feature-auth");
    }

    #[test]
    fn truncates_to_63_chars() {
        let long = "a".repeat(100);
        let result = sanitize_branch(&long);
        assert!(result.len() <= DNS_LABEL_MAX);
        assert_eq!(result.len(), DNS_LABEL_MAX);
    }

    #[test]
    fn truncation_strips_trailing_hyphen() {
        // 62 'a's + "/" puts a hyphen at position 63, which gets stripped
        let branch = format!("{}/rest", "a".repeat(62));
        let result = sanitize_branch(&branch);
        assert!(result.len() <= DNS_LABEL_MAX);
        assert!(!result.ends_with('-'));
    }

    #[test]
    fn main_branch() {
        assert_eq!(sanitize_branch("main"), "main");
    }

    #[test]
    fn empty_input() {
        assert_eq!(sanitize_branch(""), "");
    }

    #[test]
    fn only_separators() {
        assert_eq!(sanitize_branch("///"), "");
    }

    #[test]
    fn special_characters_replaced() {
        assert_eq!(sanitize_branch("feat@v2#patch"), "feat-v2-patch");
    }

    #[test]
    fn collision_suffix_appended() {
        let result = sanitize_branch_with_suffix("feature/auth");
        assert!(result.starts_with("feature-auth-"));
        assert_eq!(result.len(), "feature-auth-".len() + 4);
    }

    #[test]
    fn collision_suffix_different_for_different_branches() {
        let a = sanitize_branch_with_suffix("a/b");
        let b = sanitize_branch_with_suffix("a-b");
        assert_ne!(a, b);
    }

    #[test]
    fn collision_suffix_fits_dns_limit() {
        let long = "a".repeat(100);
        let result = sanitize_branch_with_suffix(&long);
        assert!(result.len() <= DNS_LABEL_MAX);
    }

    #[test]
    fn collision_suffix_deterministic() {
        let a = sanitize_branch_with_suffix("feature/auth");
        let b = sanitize_branch_with_suffix("feature/auth");
        assert_eq!(a, b);
    }

    // -- parse_hostname tests --

    #[test]
    fn parse_port_branch_project_localhost() {
        let parsed = parse_hostname("3000.feature-auth.myapp.localhost").unwrap();
        assert_eq!(parsed.port, Some(3000));
        assert_eq!(parsed.branch, "feature-auth");
        assert_eq!(parsed.project, "myapp");
    }

    #[test]
    fn parse_bare_hostname_localhost() {
        let parsed = parse_hostname("feature-auth.myapp.localhost").unwrap();
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.branch, "feature-auth");
        assert_eq!(parsed.project, "myapp");
    }

    #[test]
    fn parse_port_branch_project_local() {
        let parsed = parse_hostname("3000.feature-auth.myapp.local").unwrap();
        assert_eq!(parsed.port, Some(3000));
        assert_eq!(parsed.branch, "feature-auth");
        assert_eq!(parsed.project, "myapp");
    }

    #[test]
    fn parse_bare_hostname_local() {
        let parsed = parse_hostname("feature-auth.myapp.local").unwrap();
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.branch, "feature-auth");
        assert_eq!(parsed.project, "myapp");
    }

    #[test]
    fn parse_strips_host_port_suffix() {
        let parsed = parse_hostname("3000.main.myapp.localhost:80").unwrap();
        assert_eq!(parsed.port, Some(3000));
        assert_eq!(parsed.branch, "main");
    }

    #[test]
    fn parse_rejects_plain_localhost() {
        assert!(parse_hostname("localhost").is_none());
    }

    #[test]
    fn parse_rejects_single_label_before_tld() {
        assert!(parse_hostname("myapp.localhost").is_none());
    }

    #[test]
    fn parse_rejects_unknown_tld() {
        assert!(parse_hostname("3000.main.myapp.example.com").is_none());
    }

    #[test]
    fn parse_rejects_too_many_labels() {
        assert!(parse_hostname("extra.3000.main.myapp.localhost").is_none());
    }

    #[test]
    fn parse_rejects_non_numeric_port_label() {
        // "abc" is not a valid port, so 3-label form fails
        assert!(parse_hostname("abc.main.myapp.localhost").is_none());
    }

    #[test]
    fn parse_rejects_something_dot_local() {
        // "something.local" is only 2 parts total — not enough labels
        assert!(parse_hostname("something.local").is_none());
    }

    // -- build_hostname_url tests --

    #[test]
    fn build_url_with_default_proxy_port() {
        assert_eq!(
            build_hostname_url(3000, "feature/auth", "myapp", Some(80)),
            Some("http://3000.feature-auth.myapp.localhost".to_string())
        );
    }

    #[test]
    fn build_url_returns_none_without_proxy() {
        assert_eq!(
            build_hostname_url(3000, "feature/auth", "myapp", None),
            None
        );
    }

    #[test]
    fn build_url_includes_fallback_proxy_port() {
        assert_eq!(
            build_hostname_url(3000, "feature/auth", "myapp", Some(49180)),
            Some("http://3000.feature-auth.myapp.localhost:49180".to_string())
        );
    }

    #[test]
    fn build_bare_non_orbstack() {
        assert_eq!(
            build_bare_hostname("main", "myapp", false),
            "main.myapp.localhost"
        );
    }

    #[test]
    fn build_bare_orbstack() {
        assert_eq!(
            build_bare_hostname("main", "myapp", true),
            "main.myapp.local"
        );
    }
}
