//! Branch name to filesystem-safe directory name conversion.

/// Convert a branch name to a filesystem-safe directory name.
///
/// Replaces `/` with `-`, collapses consecutive dashes, and trims
/// leading/trailing dashes.
///
/// # Examples
///
/// ```
/// # use cella_git::branch_to_dir_name;
/// assert_eq!(branch_to_dir_name("feature/auth/oauth2"), "feature-auth-oauth2");
/// assert_eq!(branch_to_dir_name("main"), "main");
/// ```
pub fn branch_to_dir_name(branch: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_branch_unchanged() {
        assert_eq!(branch_to_dir_name("main"), "main");
    }

    #[test]
    fn slashes_replaced() {
        assert_eq!(
            branch_to_dir_name("feature/auth/oauth2"),
            "feature-auth-oauth2"
        );
    }

    #[test]
    fn double_slashes_collapsed() {
        assert_eq!(
            branch_to_dir_name("feature//double-slash"),
            "feature-double-slash"
        );
    }

    #[test]
    fn leading_slash_trimmed() {
        assert_eq!(branch_to_dir_name("/leading-slash"), "leading-slash");
    }

    #[test]
    fn trailing_slash_trimmed() {
        assert_eq!(branch_to_dir_name("trailing/"), "trailing");
    }

    #[test]
    fn single_segment() {
        assert_eq!(branch_to_dir_name("hotfix"), "hotfix");
    }

    #[test]
    fn deeply_nested() {
        assert_eq!(branch_to_dir_name("a/b/c/d/e"), "a-b-c-d-e");
    }
}
