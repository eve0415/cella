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
}
