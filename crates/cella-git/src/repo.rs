//! Repository discovery, default branch detection, and container detection.

use std::path::{Path, PathBuf};

use crate::CellaGitError;
use crate::cmd;

/// Information about a discovered git repository.
#[derive(Debug, Clone)]
pub struct RepoInfo {
    /// Absolute path to the repository root (where `.git` lives).
    pub root: PathBuf,
    /// The current branch name, or `None` if HEAD is detached.
    pub head_branch: Option<String>,
}

/// Discover the git repository containing `path`.
///
/// Returns the repo root and current HEAD branch.
///
/// # Errors
///
/// Returns `CellaGitError::NotARepository` if the path is not inside a git repo.
pub fn discover(path: &Path) -> Result<RepoInfo, CellaGitError> {
    let root_str = cmd::run(path, &["rev-parse", "--show-toplevel"]).map_err(|e| match e {
        CellaGitError::CommandFailed { .. } => CellaGitError::NotARepository {
            path: path.to_path_buf(),
        },
        other => other,
    })?;

    let root = PathBuf::from(root_str.trim());

    let head_branch = cmd::run(&root, &["symbolic-ref", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string());

    Ok(RepoInfo { root, head_branch })
}

/// Detect the default branch name (main, master, etc.).
///
/// Resolution priority:
/// 1. `refs/remotes/origin/HEAD` symbolic ref
/// 2. Check if `main` exists locally
/// 3. Check if `master` exists locally
/// 4. `git config init.defaultBranch`
/// 5. Fallback: `"main"`
///
/// # Errors
///
/// Returns `CellaGitError` if git commands fail unexpectedly.
pub fn default_branch(repo_root: &Path) -> Result<String, CellaGitError> {
    // Try origin/HEAD first
    if let Ok(output) = cmd::run(repo_root, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        let trimmed = output.trim();
        if let Some(branch) = trimmed.strip_prefix("refs/remotes/origin/") {
            return Ok(branch.to_string());
        }
    }

    // Check if main exists
    if cmd::run_quiet(
        repo_root,
        &["show-ref", "--verify", "--quiet", "refs/heads/main"],
    )? {
        return Ok("main".to_string());
    }

    // Check if master exists
    if cmd::run_quiet(
        repo_root,
        &["show-ref", "--verify", "--quiet", "refs/heads/master"],
    )? {
        return Ok("master".to_string());
    }

    // Check git config
    if let Ok(output) = cmd::run(repo_root, &["config", "init.defaultBranch"]) {
        let trimmed = output.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    // Final fallback
    Ok("main".to_string())
}

/// Detect if the current process is running inside a Docker container.
///
/// Checks `/.dockerenv` existence as the primary signal (cgroupv2-safe).
pub fn is_inside_container() -> bool {
    Path::new("/.dockerenv").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use crate::test_utils::init_repo;

    #[test]
    fn discover_from_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let info = discover(tmp.path()).unwrap();
        // Canonicalize both paths for comparison (handles /tmp vs /private/tmp on macOS)
        assert_eq!(
            info.root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
        // Default branch from git init is typically main or master
        assert!(info.head_branch.is_some());
    }

    #[test]
    fn discover_from_subdirectory() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let subdir = tmp.path().join("sub").join("dir");
        std::fs::create_dir_all(&subdir).unwrap();

        let info = discover(&subdir).unwrap();
        assert_eq!(
            info.root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_not_a_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = discover(tmp.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CellaGitError::NotARepository { .. }
        ));
    }

    #[test]
    fn default_branch_with_main() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Rename current branch to main
        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let branch = default_branch(tmp.path()).unwrap();
        assert_eq!(branch, "main");
    }

    #[test]
    fn default_branch_with_master() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Rename current branch to master, ensure no 'main' exists
        Command::new("git")
            .args(["branch", "-M", "master"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let branch = default_branch(tmp.path()).unwrap();
        assert_eq!(branch, "master");
    }

    #[test]
    fn is_inside_container_reflects_environment() {
        // In a devcontainer, /.dockerenv exists. Outside, it doesn't.
        // We just verify the function doesn't panic.
        let _result = is_inside_container();
    }

    #[test]
    fn discover_detached_head() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Detach HEAD
        Command::new("git")
            .args(["checkout", "--detach"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let info = discover(tmp.path()).unwrap();
        assert!(info.head_branch.is_none());
    }

    #[test]
    fn discover_returns_correct_branch_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        Command::new("git")
            .args(["checkout", "-b", "feature/test"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let info = discover(tmp.path()).unwrap();
        assert_eq!(info.head_branch.as_deref(), Some("feature/test"));
    }

    #[test]
    fn default_branch_fallback_to_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Rename to something unusual so main/master checks fail
        Command::new("git")
            .args(["branch", "-M", "develop"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // Set the init.defaultBranch config
        Command::new("git")
            .args(["config", "init.defaultBranch", "develop"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let branch = default_branch(tmp.path()).unwrap();
        assert_eq!(branch, "develop");
    }

    #[test]
    fn default_branch_final_fallback() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Rename to something unusual
        Command::new("git")
            .args(["branch", "-M", "trunk"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // Unset init.defaultBranch if it's set
        let _ = Command::new("git")
            .args(["config", "--unset", "init.defaultBranch"])
            .current_dir(tmp.path())
            .output();

        let branch = default_branch(tmp.path()).unwrap();
        // Falls back to "main" when nothing matches
        assert_eq!(branch, "main");
    }

    #[test]
    fn repo_info_clone() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let info = discover(tmp.path()).unwrap();
        let cloned = info.clone();
        assert_eq!(
            info.root.canonicalize().unwrap(),
            cloned.root.canonicalize().unwrap()
        );
        assert_eq!(info.head_branch, cloned.head_branch);
    }
}
