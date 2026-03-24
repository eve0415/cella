//! Branch resolution, tracking, and merge detection.

use std::path::Path;

use crate::CellaGitError;
use crate::cmd;

/// The resolution state of a branch name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchState {
    /// Branch exists locally.
    Local,
    /// Branch exists only on a remote.
    Remote { remote: String },
    /// Branch does not exist anywhere — it will be created new.
    New,
}

/// Resolve where a branch name exists.
///
/// Checks local branches first, then remote tracking branches.
/// Returns `New` if the branch doesn't exist anywhere.
///
/// # Errors
///
/// Returns `CellaGitError` if git commands fail.
pub fn resolve_branch(repo_root: &Path, branch: &str) -> Result<BranchState, CellaGitError> {
    // Check local
    let local_ref = format!("refs/heads/{branch}");
    if cmd::run_quiet(repo_root, &["show-ref", "--verify", "--quiet", &local_ref])? {
        return Ok(BranchState::Local);
    }

    // Check remotes
    let pattern = format!("*/{branch}");
    if let Ok(output) = cmd::run(repo_root, &["branch", "-r", "--list", &pattern]) {
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.contains("->") {
                continue;
            }
            // Extract remote name: "origin/feature-x" → "origin"
            if let Some(remote) = trimmed.split('/').next() {
                return Ok(BranchState::Remote {
                    remote: remote.to_string(),
                });
            }
        }
    }

    Ok(BranchState::New)
}

/// List branches that have been fully merged into the given base branch.
///
/// Excludes the base branch itself from the result.
///
/// # Errors
///
/// Returns `CellaGitError` if `git branch --merged` fails.
pub fn merged_branches(repo_root: &Path, base: &str) -> Result<Vec<String>, CellaGitError> {
    let output = cmd::run(repo_root, &["branch", "--merged", base])?;

    let branches = output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim().trim_start_matches("* ");
            if trimmed.is_empty() || trimmed == base {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect();

    Ok(branches)
}

/// Check if a branch's remote tracking ref is gone (deleted on remote).
///
/// Returns `false` if the branch has no upstream or if it cannot be determined.
///
/// # Errors
///
/// Returns `CellaGitError` if `git for-each-ref` fails.
pub fn is_tracking_gone(repo_root: &Path, branch: &str) -> Result<bool, CellaGitError> {
    let format_arg = "%(upstream:track)";
    let ref_arg = format!("refs/heads/{branch}");
    let output = cmd::run(
        repo_root,
        &["for-each-ref", &format!("--format={format_arg}"), &ref_arg],
    )?;

    Ok(output.trim().contains("[gone]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use crate::test_utils::init_repo;

    #[test]
    fn resolve_local_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Create a local branch
        Command::new("git")
            .args(["branch", "feature-local"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let state = resolve_branch(tmp.path(), "feature-local").unwrap();
        assert_eq!(state, BranchState::Local);
    }

    #[test]
    fn resolve_nonexistent_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        let state = resolve_branch(tmp.path(), "nonexistent").unwrap();
        assert_eq!(state, BranchState::New);
    }

    #[test]
    fn merged_branches_includes_merged() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // Ensure we're on main
        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // Create and merge a feature branch
        Command::new("git")
            .args(["checkout", "-b", "feature-done"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "feature work"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["merge", "feature-done"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let merged = merged_branches(tmp.path(), "main").unwrap();
        assert!(merged.contains(&"feature-done".to_string()));
    }

    #[test]
    fn merged_branches_excludes_unmerged() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // Create an unmerged branch
        Command::new("git")
            .args(["checkout", "-b", "feature-wip"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "wip"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let merged = merged_branches(tmp.path(), "main").unwrap();
        assert!(!merged.contains(&"feature-wip".to_string()));
    }

    #[test]
    fn tracking_gone_no_remote() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        // A branch without upstream is not "gone"
        Command::new("git")
            .args(["branch", "local-only"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let gone = is_tracking_gone(tmp.path(), "local-only").unwrap();
        assert!(!gone);
    }

    #[test]
    fn resolve_current_head_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // The current HEAD branch should resolve as Local
        let state = resolve_branch(tmp.path(), "main").unwrap();
        assert_eq!(state, BranchState::Local);
    }

    #[test]
    fn merged_branches_excludes_base() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let merged = merged_branches(tmp.path(), "main").unwrap();
        // "main" should not be in its own merged list
        assert!(!merged.contains(&"main".to_string()));
    }

    #[test]
    fn merged_branches_multiple() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_repo(tmp.path());

        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // Create and merge two branches
        for name in &["feat-a", "feat-b"] {
            Command::new("git")
                .args(["checkout", "-b", name])
                .current_dir(tmp.path())
                .output()
                .unwrap();
            Command::new("git")
                .args(["commit", "--allow-empty", "-m", &format!("{name} work")])
                .current_dir(tmp.path())
                .output()
                .unwrap();
            Command::new("git")
                .args(["checkout", "main"])
                .current_dir(tmp.path())
                .output()
                .unwrap();
            Command::new("git")
                .args(["merge", name])
                .current_dir(tmp.path())
                .output()
                .unwrap();
        }

        let merged = merged_branches(tmp.path(), "main").unwrap();
        assert!(merged.contains(&"feat-a".to_string()));
        assert!(merged.contains(&"feat-b".to_string()));
    }

    #[test]
    fn branch_state_debug_display() {
        assert_eq!(format!("{:?}", BranchState::Local), "Local");
        assert_eq!(
            format!(
                "{:?}",
                BranchState::Remote {
                    remote: "origin".to_string()
                }
            ),
            "Remote { remote: \"origin\" }"
        );
        assert_eq!(format!("{:?}", BranchState::New), "New");
    }

    #[test]
    fn branch_state_equality() {
        assert_eq!(BranchState::Local, BranchState::Local);
        assert_eq!(BranchState::New, BranchState::New);
        assert_eq!(
            BranchState::Remote {
                remote: "origin".to_string()
            },
            BranchState::Remote {
                remote: "origin".to_string()
            }
        );
        assert_ne!(BranchState::Local, BranchState::New);
        assert_ne!(
            BranchState::Remote {
                remote: "origin".to_string()
            },
            BranchState::Remote {
                remote: "upstream".to_string()
            }
        );
    }
}
