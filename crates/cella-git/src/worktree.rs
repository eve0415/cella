//! Git worktree create, list, remove, and path computation.

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::CellaGitError;
use crate::branch::BranchState;
use crate::cmd;
use crate::sanitize::branch_to_dir_name;

/// Information about a single git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// The HEAD commit hash.
    pub head: String,
    /// The branch checked out, or `None` if HEAD is detached.
    pub branch: Option<String>,
    /// Whether this is the main worktree (not a linked worktree).
    pub is_main: bool,
}

/// If the workspace is a linked git worktree, return the parent repo's `.git`
/// directory path.
///
/// Linked worktrees have a `.git` *file* (not directory) containing a `gitdir:`
/// pointer to `<parent-repo>/.git/worktrees/<name>`. This function reads that
/// pointer and returns the parent `.git` directory (two levels up from the
/// pointed-to path).
///
/// Returns `None` if the workspace is a regular repo (`.git` is a directory or
/// doesn't exist).
pub fn parent_git_dir(workspace_root: &Path) -> Option<PathBuf> {
    let dot_git = workspace_root.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = content.strip_prefix("gitdir: ")?.trim();
    let gitdir_path = PathBuf::from(gitdir);
    // gitdir points to .git/worktrees/<name> — parent .git is two levels up
    gitdir_path.parent()?.parent().map(PathBuf::from)
}

/// Compute the worktree directory path for a branch.
///
/// If `worktree_root` is provided (from `cella.toml` config), the worktree
/// is placed under that root. Otherwise, the default sibling pattern is used:
/// `{repo_parent}/{repo_dir_name}-worktrees/{sanitized_branch}`.
pub fn worktree_path(repo_root: &Path, branch: &str, worktree_root: Option<&Path>) -> PathBuf {
    let dir_name = branch_to_dir_name(branch);
    worktree_root.map_or_else(
        || {
            let repo_name = repo_root
                .file_name()
                .map_or("repo", |n| n.to_str().unwrap_or("repo"));
            let parent = repo_root.parent().unwrap_or(repo_root);
            parent
                .join(format!("{repo_name}-worktrees"))
                .join(&dir_name)
        },
        |root| root.join(&dir_name),
    )
}

/// Create a worktree for the given branch.
///
/// - If `branch_state` is `New`, creates the branch from `base` (or HEAD).
/// - If `Remote`, creates a local branch tracking the remote.
/// - If `Local`, checks it out in the new worktree.
///
/// Returns information about the created worktree.
///
/// # Errors
///
/// Returns `CellaGitError::WorktreeAlreadyExists` if the path exists,
/// `CellaGitError::BranchCheckedOut` if the branch is checked out elsewhere,
/// or other git errors.
pub fn create(
    repo_root: &Path,
    branch: &str,
    branch_state: &BranchState,
    worktree_root: Option<&Path>,
    base: Option<&str>,
) -> Result<WorktreeInfo, CellaGitError> {
    let wt_path = worktree_path(repo_root, branch, worktree_root);

    if wt_path.exists() {
        return Err(CellaGitError::WorktreeAlreadyExists { path: wt_path });
    }

    let wt_path_str = wt_path.to_string_lossy();

    let result = match branch_state {
        BranchState::New => {
            let mut args = vec!["worktree", "add", "-b", branch, &wt_path_str];
            if let Some(b) = base {
                args.push(b);
            }
            cmd::run(repo_root, &args)
        }
        BranchState::Remote { remote } => {
            let remote_ref = format!("{remote}/{branch}");
            cmd::run(
                repo_root,
                &["worktree", "add", "-b", branch, &wt_path_str, &remote_ref],
            )
        }
        BranchState::Local => cmd::run(repo_root, &["worktree", "add", &wt_path_str, branch]),
    };

    match result {
        Ok(_) => {
            debug!("created worktree at {}", wt_path.display());

            // Set up remote tracking for branches created from remote refs
            if let BranchState::Remote { remote } = branch_state {
                let upstream = format!("{remote}/{branch}");
                let _ = cmd::run(
                    &wt_path,
                    &["branch", "--set-upstream-to", &upstream, branch],
                );
            }

            // Find and return the worktree info
            let worktrees = list(repo_root)?;
            worktrees
                .into_iter()
                .find(|wt| {
                    wt.path.canonicalize().unwrap_or_else(|_| wt.path.clone())
                        == wt_path.canonicalize().unwrap_or_else(|_| wt_path.clone())
                })
                .ok_or_else(|| CellaGitError::ParseError {
                    context: format!(
                        "worktree created but not found in list: {}",
                        wt_path.display()
                    ),
                })
        }
        Err(CellaGitError::CommandFailed { stderr, .. })
            if stderr.contains("already checked out")
                || stderr.contains("already used by worktree") =>
        {
            // Parse the path from git's error: "fatal: 'branch' is already checked out at '/path'"
            let checked_out_path = stderr
                .split('\'')
                .nth(3)
                .map(PathBuf::from)
                .unwrap_or_default();
            Err(CellaGitError::BranchCheckedOut {
                branch: branch.to_string(),
                worktree_path: checked_out_path,
            })
        }
        Err(e) => Err(e),
    }
}

/// List all worktrees for the repository.
///
/// Parses `git worktree list --porcelain` output.
///
/// # Errors
///
/// Returns `CellaGitError` if `git worktree list` fails.
pub fn list(repo_root: &Path) -> Result<Vec<WorktreeInfo>, CellaGitError> {
    let output = cmd::run(repo_root, &["worktree", "list", "--porcelain"])?;
    Ok(parse_porcelain_output(&output))
}

/// Remove a worktree by path.
///
/// # Errors
///
/// Returns `CellaGitError::WorktreeNotFound` if the path is not a worktree,
/// or other git errors.
pub fn remove(repo_root: &Path, wt_path: &Path) -> Result<(), CellaGitError> {
    let path_str = wt_path.to_string_lossy();
    cmd::run(repo_root, &["worktree", "remove", "--force", &path_str]).map_err(|e| match e {
        CellaGitError::CommandFailed { ref stderr, .. }
            if stderr.contains("is not a working tree") =>
        {
            CellaGitError::WorktreeNotFound {
                path: wt_path.to_path_buf(),
            }
        }
        other => other,
    })?;
    debug!("removed worktree at {}", wt_path.display());
    Ok(())
}

/// Parse `git worktree list --porcelain` output into structured entries.
fn parse_porcelain_output(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head: Option<String> = None;
    let mut current_branch: Option<String> = None;
    let mut is_bare = false;
    let mut is_first = true;

    for line in output.lines() {
        if line.is_empty() {
            // End of entry
            if let (Some(path), Some(head)) = (current_path.take(), current_head.take()) {
                worktrees.push(WorktreeInfo {
                    path,
                    head,
                    branch: current_branch.take(),
                    is_main: is_first && !is_bare,
                });
                is_first = false;
                is_bare = false;
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current_head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            // "refs/heads/main" → "main"
            let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
            current_branch = Some(short.to_string());
        } else if line == "bare" {
            is_bare = true;
        }
        // "detached" line means no branch — current_branch stays None
    }

    // Handle last entry (may not have trailing blank line)
    if let (Some(path), Some(head)) = (current_path, current_head) {
        worktrees.push(WorktreeInfo {
            path,
            head,
            branch: current_branch,
            is_main: is_first && !is_bare,
        });
    }

    worktrees
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    use crate::test_utils::init_repo;

    #[test]
    fn worktree_path_default_sibling() {
        let repo = Path::new("/home/user/my-project");
        let path = worktree_path(repo, "feature/auth", None);
        assert_eq!(
            path,
            PathBuf::from("/home/user/my-project-worktrees/feature-auth")
        );
    }

    #[test]
    fn worktree_path_custom_root() {
        let repo = Path::new("/home/user/my-project");
        let custom = Path::new("/tmp/worktrees");
        let path = worktree_path(repo, "feature/auth", Some(custom));
        assert_eq!(path, PathBuf::from("/tmp/worktrees/feature-auth"));
    }

    #[test]
    fn list_fresh_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let worktrees = list(tmp.path()).unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0].is_main);
        assert!(worktrees[0].branch.is_some());
    }

    #[test]
    fn create_and_list_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        let info = create(
            tmp.path(),
            "test-branch",
            &BranchState::New,
            Some(&wt_root),
            None,
        )
        .unwrap();

        assert_eq!(info.branch.as_deref(), Some("test-branch"));
        assert!(!info.is_main);
        assert!(info.path.exists());

        let worktrees = list(tmp.path()).unwrap();
        assert_eq!(worktrees.len(), 2);
    }

    #[test]
    fn create_and_remove() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        let info = create(
            tmp.path(),
            "to-remove",
            &BranchState::New,
            Some(&wt_root),
            None,
        )
        .unwrap();

        assert!(info.path.exists());

        remove(tmp.path(), &info.path).unwrap();

        let worktrees = list(tmp.path()).unwrap();
        assert_eq!(worktrees.len(), 1);
    }

    #[test]
    fn create_existing_local_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Create a local branch first
        StdCommand::new("git")
            .args(["branch", "existing-branch"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        let info = create(
            tmp.path(),
            "existing-branch",
            &BranchState::Local,
            Some(&wt_root),
            None,
        )
        .unwrap();

        assert_eq!(info.branch.as_deref(), Some("existing-branch"));
        assert!(info.path.exists());
    }

    #[test]
    fn create_duplicate_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        create(
            tmp.path(),
            "dup-branch",
            &BranchState::New,
            Some(&wt_root),
            None,
        )
        .unwrap();

        let result = create(
            tmp.path(),
            "dup-branch",
            &BranchState::New,
            Some(&wt_root),
            None,
        );

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CellaGitError::WorktreeAlreadyExists { .. }
        ));
    }

    #[test]
    fn create_with_base() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        StdCommand::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        let info = create(
            tmp.path(),
            "from-base",
            &BranchState::New,
            Some(&wt_root),
            Some("main"),
        )
        .unwrap();

        assert_eq!(info.branch.as_deref(), Some("from-base"));
    }

    #[test]
    fn parse_porcelain_basic() {
        let output = "\
worktree /home/user/repo
HEAD abc123def456
branch refs/heads/main

worktree /home/user/repo-worktrees/feature
HEAD def456abc789
branch refs/heads/feature

";
        let worktrees = parse_porcelain_output(output);
        assert_eq!(worktrees.len(), 2);

        assert_eq!(worktrees[0].path, PathBuf::from("/home/user/repo"));
        assert_eq!(worktrees[0].head, "abc123def456");
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(worktrees[0].is_main);

        assert_eq!(
            worktrees[1].path,
            PathBuf::from("/home/user/repo-worktrees/feature")
        );
        assert_eq!(worktrees[1].branch.as_deref(), Some("feature"));
        assert!(!worktrees[1].is_main);
    }

    #[test]
    fn parse_porcelain_detached_head() {
        let output = "\
worktree /home/user/repo
HEAD abc123def456
detached

";
        let worktrees = parse_porcelain_output(output);
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0].branch.is_none());
    }

    #[test]
    fn parse_porcelain_no_trailing_newline() {
        let output = "\
worktree /home/user/repo
HEAD abc123def456
branch refs/heads/main";
        let worktrees = parse_porcelain_output(output);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_porcelain_bare_repo() {
        let output = "\
worktree /home/user/repo
HEAD abc123def456
bare

worktree /home/user/repo-worktrees/feature
HEAD def456abc789
branch refs/heads/feature

";
        let worktrees = parse_porcelain_output(output);
        assert_eq!(worktrees.len(), 2);
        // bare worktree: is_main should be false because it's bare
        assert!(!worktrees[0].is_main);
        assert!(worktrees[0].branch.is_none());
        assert!(!worktrees[1].is_main);
    }

    #[test]
    fn parse_porcelain_empty_input() {
        let worktrees = parse_porcelain_output("");
        assert!(worktrees.is_empty());
    }

    #[test]
    fn parse_porcelain_multiple_linked_worktrees() {
        let output = "\
worktree /repo
HEAD aaa
branch refs/heads/main

worktree /repo-wt/feat-a
HEAD bbb
branch refs/heads/feat-a

worktree /repo-wt/feat-b
HEAD ccc
branch refs/heads/feat-b

worktree /repo-wt/detached
HEAD ddd
detached

";
        let worktrees = parse_porcelain_output(output);
        assert_eq!(worktrees.len(), 4);
        assert!(worktrees[0].is_main);
        assert_eq!(worktrees[1].branch.as_deref(), Some("feat-a"));
        assert_eq!(worktrees[2].branch.as_deref(), Some("feat-b"));
        assert!(worktrees[3].branch.is_none()); // detached
    }

    #[test]
    fn remove_nonexistent_worktree_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let fake_path = tmp.path().join("does-not-exist");
        let result = remove(tmp.path(), &fake_path);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CellaGitError::WorktreeNotFound { .. }
        ));
    }

    #[test]
    fn create_multiple_worktrees() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        create(
            tmp.path(),
            "branch-a",
            &BranchState::New,
            Some(&wt_root),
            None,
        )
        .unwrap();
        create(
            tmp.path(),
            "branch-b",
            &BranchState::New,
            Some(&wt_root),
            None,
        )
        .unwrap();

        let worktrees = list(tmp.path()).unwrap();
        assert_eq!(worktrees.len(), 3); // main + 2 linked
    }

    #[test]
    fn worktree_path_root_repo() {
        // Repo at filesystem root edge case
        let repo = Path::new("/my-project");
        let path = worktree_path(repo, "fix/bug", None);
        assert_eq!(path, PathBuf::from("/my-project-worktrees/fix-bug"));
    }

    #[test]
    fn create_branch_checked_out_at_main() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        StdCommand::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        // Trying to create a worktree for the currently checked-out branch
        let result = create(
            tmp.path(),
            "main",
            &BranchState::Local,
            Some(&wt_root),
            None,
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CellaGitError::BranchCheckedOut { .. }
        ));
    }

    #[test]
    fn parent_git_dir_regular_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());
        // Regular repo has .git directory, not a file
        assert!(parent_git_dir(tmp.path()).is_none());
    }

    #[test]
    fn parent_git_dir_no_git() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No .git at all
        assert!(parent_git_dir(tmp.path()).is_none());
    }

    #[test]
    fn parent_git_dir_linked_worktree() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();

        let info = create(
            tmp.path(),
            "wt-test",
            &BranchState::New,
            Some(&wt_root),
            None,
        )
        .unwrap();

        let result = parent_git_dir(&info.path);
        assert!(result.is_some());
        // Should point to the parent repo's .git directory
        let parent_git = result.unwrap();
        assert_eq!(
            parent_git.canonicalize().unwrap(),
            tmp.path().join(".git").canonicalize().unwrap()
        );
    }
}
