use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur during git operations.
#[derive(Debug, Error, Diagnostic)]
pub enum CellaGitError {
    /// Git is not installed or not found in PATH.
    #[error("git not found in PATH")]
    #[diagnostic(code(cella::git::git_not_found))]
    GitNotFound,

    /// The current directory is not a git repository.
    #[error("not a git repository: {path}")]
    #[diagnostic(code(cella::git::not_a_repository))]
    NotARepository { path: PathBuf },

    /// A git command failed.
    #[error("git command failed: `git {args}`\n{stderr}")]
    #[diagnostic(code(cella::git::command_failed))]
    CommandFailed { args: String, stderr: String },

    /// Git lock was held and retries were exhausted.
    #[error("git lock held, retries exhausted: {path}")]
    #[diagnostic(code(cella::git::lock_contention))]
    LockContention { path: PathBuf },

    /// A worktree already exists at the given path.
    #[error("worktree already exists: {}", path.display())]
    #[diagnostic(code(cella::git::worktree_already_exists))]
    WorktreeAlreadyExists { path: PathBuf },

    /// No worktree was found at the given path.
    #[error("worktree not found: {}", path.display())]
    #[diagnostic(code(cella::git::worktree_not_found))]
    WorktreeNotFound { path: PathBuf },

    /// The branch is already checked out in another worktree.
    #[error("branch '{branch}' is already checked out at {}", worktree_path.display())]
    #[diagnostic(code(cella::git::branch_checked_out))]
    BranchCheckedOut {
        branch: String,
        worktree_path: PathBuf,
    },

    /// The specified branch was not found.
    #[error("branch not found: {branch}")]
    #[diagnostic(code(cella::git::branch_not_found))]
    BranchNotFound { branch: String },

    /// Git output could not be parsed.
    #[error("failed to parse git output: {context}")]
    #[diagnostic(code(cella::git::parse_error))]
    ParseError { context: String },

    /// An I/O error occurred.
    #[error(transparent)]
    #[diagnostic(code(cella::git::io))]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_not_a_repository() {
        let err = CellaGitError::NotARepository {
            path: PathBuf::from("/tmp/not-a-repo"),
        };
        insta::assert_snapshot!(err.to_string(), @"not a git repository: /tmp/not-a-repo");
    }

    #[test]
    fn error_display_worktree_already_exists() {
        let err = CellaGitError::WorktreeAlreadyExists {
            path: PathBuf::from("/tmp/my-worktree"),
        };
        insta::assert_snapshot!(err.to_string(), @"worktree already exists: /tmp/my-worktree");
    }

    #[test]
    fn error_display_branch_checked_out() {
        let err = CellaGitError::BranchCheckedOut {
            branch: "feature/auth".to_string(),
            worktree_path: PathBuf::from("/tmp/worktrees/feature-auth"),
        };
        insta::assert_snapshot!(err.to_string(), @"branch 'feature/auth' is already checked out at /tmp/worktrees/feature-auth");
    }

    #[test]
    fn error_display_command_failed() {
        let err = CellaGitError::CommandFailed {
            args: "worktree add /tmp/wt".to_string(),
            stderr: "fatal: something went wrong".to_string(),
        };
        insta::assert_snapshot!(err.to_string(), @"
        git command failed: `git worktree add /tmp/wt`
        fatal: something went wrong
        ");
    }

    #[test]
    fn error_display_lock_contention() {
        let err = CellaGitError::LockContention {
            path: PathBuf::from("/tmp/repo"),
        };
        insta::assert_snapshot!(err.to_string(), @"git lock held, retries exhausted: /tmp/repo");
    }

    #[test]
    fn error_display_branch_not_found() {
        let err = CellaGitError::BranchNotFound {
            branch: "nonexistent".to_string(),
        };
        insta::assert_snapshot!(err.to_string(), @"branch not found: nonexistent");
    }

    #[test]
    fn error_display_worktree_not_found() {
        let err = CellaGitError::WorktreeNotFound {
            path: PathBuf::from("/tmp/missing-wt"),
        };
        insta::assert_snapshot!(err.to_string(), @"worktree not found: /tmp/missing-wt");
    }

    #[test]
    fn error_display_parse_error() {
        let err = CellaGitError::ParseError {
            context: "unexpected format in worktree list".to_string(),
        };
        insta::assert_snapshot!(err.to_string(), @"failed to parse git output: unexpected format in worktree list");
    }
}
