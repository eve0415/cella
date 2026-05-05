//! Branch name to filesystem-safe directory name conversion.

use sha2::{Digest, Sha256};

/// Convert a branch name to a filesystem-safe directory name.
///
/// Replaces `/` with `-`, collapses consecutive dashes, trims
/// leading/trailing dashes, and appends a 4-char hash suffix to
/// prevent collisions (e.g. `feature/auth` vs `feature-auth`).
///
/// # Examples
///
/// ```
/// # use cella_git::branch_to_dir_name;
/// let a = branch_to_dir_name("feature/auth");
/// let b = branch_to_dir_name("feature-auth");
/// assert_ne!(a, b);
/// assert!(a.starts_with("feature-auth-"));
/// assert!(b.starts_with("feature-auth-"));
/// ```
pub fn branch_to_dir_name(branch: &str) -> String {
    let sanitized = sanitize_chars(branch);
    let hash = short_hash(branch);
    format!("{sanitized}-{hash}")
}

/// Convert a branch name without the hash suffix (for backward compatibility lookups).
pub fn branch_to_dir_name_legacy(branch: &str) -> String {
    sanitize_chars(branch)
}

fn sanitize_chars(branch: &str) -> String {
    let mut result = String::with_capacity(branch.len());
    let mut prev_dash = false;

    for c in branch.chars() {
        if c == '/' {
            if !prev_dash {
                result.push('-');
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    result.trim_matches('-').to_string()
}

fn short_hash(input: &str) -> String {
    let hash = Sha256::digest(input.as_bytes());
    hex::encode(&hash[..2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_branch_gets_hash_suffix() {
        let name = branch_to_dir_name("main");
        assert!(name.starts_with("main-"));
        assert_eq!(name.len(), "main-".len() + 4);
    }

    #[test]
    fn slashes_replaced_with_hash() {
        let name = branch_to_dir_name("feature/auth/oauth2");
        assert!(name.starts_with("feature-auth-oauth2-"));
    }

    #[test]
    fn collision_prevention() {
        let a = branch_to_dir_name("feature/auth");
        let b = branch_to_dir_name("feature-auth");
        assert_ne!(a, b);
    }

    #[test]
    fn deterministic() {
        assert_eq!(branch_to_dir_name("main"), branch_to_dir_name("main"));
    }

    #[test]
    fn double_slashes_collapsed() {
        let name = branch_to_dir_name("feature//double-slash");
        assert!(name.starts_with("feature-double-slash-"));
    }

    #[test]
    fn leading_slash_trimmed() {
        let name = branch_to_dir_name("/leading-slash");
        assert!(name.starts_with("leading-slash-"));
    }

    #[test]
    fn trailing_slash_trimmed() {
        let name = branch_to_dir_name("trailing/");
        assert!(name.starts_with("trailing-"));
    }

    #[test]
    fn deeply_nested() {
        let name = branch_to_dir_name("a/b/c/d/e");
        assert!(name.starts_with("a-b-c-d-e-"));
    }

    #[test]
    fn legacy_no_hash() {
        assert_eq!(branch_to_dir_name_legacy("feature/auth"), "feature-auth");
        assert_eq!(branch_to_dir_name_legacy("main"), "main");
    }
}
