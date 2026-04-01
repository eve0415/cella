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

    // --- find_gh_token_end edge cases ---

    #[test]
    fn find_gh_token_end_too_short() {
        // Less than 5 chars remaining
        assert_eq!(find_gh_token_end("gho_", 0), None);
        assert_eq!(find_gh_token_end("gh", 0), None);
        assert_eq!(find_gh_token_end("g", 0), None);
    }

    #[test]
    fn find_gh_token_end_no_gh_prefix() {
        assert_eq!(find_gh_token_end("abcdefghij", 0), None);
    }

    #[test]
    fn find_gh_token_end_wrong_third_char() {
        // gh followed by a char that is not o/p/s
        assert_eq!(find_gh_token_end("ghx_abcdef", 0), None);
        assert_eq!(find_gh_token_end("gha_abcdef", 0), None);
    }

    #[test]
    fn find_gh_token_end_no_underscore_fourth() {
        assert_eq!(find_gh_token_end("ghoxabcdef", 0), None);
    }

    #[test]
    fn find_gh_token_end_no_char_after_prefix() {
        // gh[ops]_ followed by non-alphanumeric
        assert_eq!(find_gh_token_end("gho_ space", 0), None);
    }

    #[test]
    fn find_gh_token_end_valid_minimal() {
        // Minimum valid token: gho_X (5 chars)
        assert_eq!(find_gh_token_end("gho_X", 0), Some(5));
    }

    #[test]
    fn find_gh_token_end_at_offset() {
        // Token starts at offset 7
        let s = "Token: gho_abc123";
        assert_eq!(find_gh_token_end(s, 7), Some(17));
        // Non-token at offset 0
        assert_eq!(find_gh_token_end(s, 0), None);
    }

    #[test]
    fn find_gh_token_end_stops_at_non_alnum() {
        assert_eq!(find_gh_token_end("ghp_abc.rest", 0), Some(7));
        assert_eq!(find_gh_token_end("ghs_token value", 0), Some(9));
    }

    #[test]
    fn find_gh_token_end_underscores_inside_token() {
        // Underscores within the token body are allowed
        assert_eq!(find_gh_token_end("gho_a_b_c", 0), Some(9));
    }

    // --- redact_tokens edge cases ---

    #[test]
    fn redact_tokens_empty_string() {
        assert_eq!(redact_tokens(""), "");
    }

    #[test]
    fn redact_tokens_multiple_tokens() {
        assert_eq!(
            redact_tokens("first ghp_aaa then ghs_bbb end"),
            "first <redacted> then <redacted> end"
        );
    }

    #[test]
    fn redact_tokens_adjacent_tokens() {
        assert_eq!(redact_tokens("gho_abc ghs_def"), "<redacted> <redacted>");
    }

    #[test]
    fn redact_tokens_token_at_end_of_string() {
        assert_eq!(redact_tokens("key=ghp_secret123"), "key=<redacted>");
    }

    // --- redact_enterprise_username_line edge cases ---

    #[test]
    fn redact_enterprise_line_no_space_after_host() {
        // "Logged in to host" with no trailing space
        let line = "Logged in to enterprise.com";
        assert_eq!(redact_enterprise_username_line(line), line);
    }

    #[test]
    fn redact_enterprise_line_no_as_keyword() {
        let line = "Logged in to enterprise.com with token";
        assert_eq!(redact_enterprise_username_line(line), line);
    }

    #[test]
    fn redact_enterprise_line_empty_username() {
        // " as " followed immediately by a parenthesis (empty username)
        let line = "Logged in to enterprise.com as (token)";
        assert_eq!(redact_enterprise_username_line(line), line);
    }

    #[test]
    fn redact_enterprise_username_multiline() {
        let input = "line1\nLogged in to ghe.corp.com as admin\nline3";
        let expected = "line1\nLogged in to ghe.corp.com as <redacted>\nline3";
        assert_eq!(redact_enterprise_username(input), expected);
    }

    #[test]
    fn redact_enterprise_username_multiple_hosts() {
        let input = "Logged in to github.com as pubuser\nLogged in to ghe.corp.com as privuser";
        let expected =
            "Logged in to github.com as pubuser\nLogged in to ghe.corp.com as <redacted>";
        assert_eq!(redact_enterprise_username(input), expected);
    }

    // --- Redactor edge cases ---

    #[test]
    fn redactor_home_dir_trailing_slash_stripped() {
        let r = redactor_with_home("/home/user");
        // Verify it was stored without trailing slash
        assert_eq!(r.home_dir.as_deref(), Some("/home/user"));
    }

    #[test]
    fn redactor_redact_home_no_home_set() {
        let r = Redactor { home_dir: None };
        assert_eq!(r.redact_home_dir("/home/user/file"), "/home/user/file");
    }

    #[test]
    fn redactor_default_creates_instance() {
        // Just verify Default trait works (coverage for Default impl)
        let _r = Redactor::default();
    }

    #[test]
    fn redact_all_three_layers_applied() {
        let r = redactor_with_home("/home/test");
        // String with home dir, a GH token, and an enterprise username
        let input = "/home/test/.config ghp_secret123 Logged in to ghe.example.com as admin";
        let result = r.redact(input);
        assert!(result.contains("~/.config"), "home should be redacted");
        assert!(result.contains("<redacted>"), "token should be redacted");
        assert!(
            !result.contains("admin"),
            "enterprise username should be redacted"
        );
    }
}
