//! Redaction of sensitive information from diagnostic output.
//!
//! Replaces home directory paths with `~/`, tokens with `<redacted>`,
//! and GitHub Enterprise usernames with `<redacted>`.

/// Handles redaction of sensitive data in diagnostic output.
pub struct Redactor {
    /// Resolved home directory (from `$HOME`), without trailing slash.
    home_dir: Option<String>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    /// Create a new redactor, resolving `$HOME` from the environment.
    pub fn new() -> Self {
        let home_dir = std::env::var("HOME")
            .ok()
            .map(|h| h.trim_end_matches('/').to_string())
            .filter(|h| !h.is_empty());
        Self { home_dir }
    }

    /// Apply all redaction rules to a string.
    pub fn redact(&self, s: &str) -> String {
        let s = self.redact_home_dir(s);
        let s = redact_tokens(&s);
        redact_enterprise_username(&s)
    }

    /// Replace home directory prefix with `~`.
    fn redact_home_dir(&self, s: &str) -> String {
        let Some(ref home) = self.home_dir else {
            return s.to_string();
        };
        s.replace(home, "~")
    }
}

/// Redact GitHub CLI tokens (`gho_`, `ghp_`, `ghs_` prefixed).
fn redact_tokens(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();

    while let Some((i, _)) = chars.peek().copied() {
        if let Some(token_end) = find_gh_token_end(s, i) {
            result.push_str("<redacted>");
            // Skip past the token
            while chars.peek().is_some_and(|&(j, _)| j < token_end) {
                chars.next();
            }
        } else {
            result.push(s.as_bytes()[i] as char);
            chars.next();
        }
    }
    result
}

/// If position `i` starts a `gh[ops]_` token, return the end index.
fn find_gh_token_end(s: &str, i: usize) -> Option<usize> {
    let remaining = &s[i..];
    if remaining.len() < 5 {
        return None;
    }
    if !remaining.starts_with("gh") {
        return None;
    }
    let third = remaining.as_bytes().get(2)?;
    if !matches!(third, b'o' | b'p' | b's') {
        return None;
    }
    if remaining.as_bytes().get(3)? != &b'_' {
        return None;
    }
    // Token must have at least one char after gh[ops]_
    if remaining.len() < 5 || !remaining.as_bytes()[4].is_ascii_alphanumeric() {
        return None;
    }
    // Find the end of the token (alphanumeric + underscore)
    let token_end = remaining[4..]
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map_or(remaining.len(), |pos| pos + 4);
    Some(i + token_end)
}

/// Redact GitHub Enterprise usernames from `gh auth status` output.
///
/// Preserves usernames for `github.com` (public), redacts for other hosts.
fn redact_enterprise_username(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for line in s.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&redact_enterprise_username_line(line));
    }
    result
}

fn redact_enterprise_username_line(line: &str) -> String {
    // Match pattern: "Logged in to <host> as <username>"
    let Some(logged_idx) = line.find("Logged in to ") else {
        return line.to_string();
    };
    let after_prefix = &line[logged_idx + "Logged in to ".len()..];

    // Extract host (next word)
    let Some(space_after_host) = after_prefix.find(' ') else {
        return line.to_string();
    };
    let host = &after_prefix[..space_after_host];

    // If it's github.com, keep the username
    if host == "github.com" {
        return line.to_string();
    }

    // Find " as " after the host
    let rest = &after_prefix[space_after_host..];
    let Some(as_idx) = rest.find(" as ") else {
        return line.to_string();
    };

    // Extract username (word after " as ")
    let after_as = &rest[as_idx + " as ".len()..];
    let username_end = after_as
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(after_as.len());

    if username_end == 0 {
        return line.to_string();
    }

    // Reconstruct with <redacted> replacing the username
    let prefix =
        &line[..logged_idx + "Logged in to ".len() + space_after_host + as_idx + " as ".len()];
    let suffix = &after_as[username_end..];
    format!("{prefix}<redacted>{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor_with_home(home: &str) -> Redactor {
        Redactor {
            home_dir: Some(home.to_string()),
        }
    }

    #[test]
    fn redact_home_linux() {
        let r = redactor_with_home("/home/john");
        assert_eq!(
            r.redact("/home/john/project/file.json"),
            "~/project/file.json"
        );
    }

    #[test]
    fn redact_home_macos() {
        let r = redactor_with_home("/Users/john");
        assert_eq!(r.redact("/Users/john/.config/gh"), "~/.config/gh");
    }

    #[test]
    fn redact_home_multiple_occurrences() {
        let r = redactor_with_home("/home/user");
        assert_eq!(r.redact("/home/user/a, /home/user/b"), "~/a, ~/b");
    }

    #[test]
    fn redact_no_home_match() {
        let r = redactor_with_home("/home/john");
        assert_eq!(r.redact("/var/run/docker.sock"), "/var/run/docker.sock");
    }

    #[test]
    fn redact_no_home_set() {
        let r = Redactor { home_dir: None };
        assert_eq!(r.redact("/home/john/foo"), "/home/john/foo");
    }

    #[test]
    fn redact_token_gho() {
        assert_eq!(redact_tokens("Token: gho_abc123XYZ"), "Token: <redacted>");
    }

    #[test]
    fn redact_token_ghp() {
        assert_eq!(redact_tokens("ghp_abc123"), "<redacted>");
    }

    #[test]
    fn redact_token_ghs() {
        assert_eq!(
            redact_tokens("prefix ghs_token_value suffix"),
            "prefix <redacted> suffix"
        );
    }

    #[test]
    fn redact_token_not_a_token() {
        assert_eq!(
            redact_tokens("ghost is not a token"),
            "ghost is not a token"
        );
        assert_eq!(redact_tokens("ghx_abc"), "ghx_abc");
    }

    #[test]
    fn redact_enterprise_username_github_com() {
        let input = "Logged in to github.com as john-doe (/home/x/.config/gh/hosts.yml)";
        assert_eq!(
            redact_enterprise_username(input),
            input,
            "github.com usernames should not be redacted"
        );
    }

    #[test]
    fn redact_enterprise_username_ghe() {
        let input = "Logged in to ghe.corp.com as john-doe (/home/x/.config/gh/hosts.yml)";
        assert_eq!(
            redact_enterprise_username(input),
            "Logged in to ghe.corp.com as <redacted> (/home/x/.config/gh/hosts.yml)"
        );
    }

    #[test]
    fn redact_enterprise_username_custom_host() {
        let input = "  Logged in to github.example.com as admin";
        assert_eq!(
            redact_enterprise_username(input),
            "  Logged in to github.example.com as <redacted>"
        );
    }

    #[test]
    fn redact_no_logged_in_line() {
        let input = "Token scopes: repo, read:org";
        assert_eq!(redact_enterprise_username(input), input);
    }

    #[test]
    fn redact_combined() {
        let r = redactor_with_home("/home/john");
        let input = "Logged in to ghe.corp.com as john (/home/john/.config/gh/hosts.yml)\nToken: gho_abc123";
        let expected =
            "Logged in to ghe.corp.com as <redacted> (~/.config/gh/hosts.yml)\nToken: <redacted>";
        assert_eq!(r.redact(input), expected);
    }

    #[test]
    fn redact_combined_github_com() {
        let r = redactor_with_home("/home/john");
        let input = "Logged in to github.com as john (/home/john/.config/gh/hosts.yml)";
        let expected = "Logged in to github.com as john (~/.config/gh/hosts.yml)";
        assert_eq!(r.redact(input), expected);
    }
}
