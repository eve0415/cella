use thiserror::Error;

/// Errors that can occur during git operations.
#[derive(Debug, Error)]
pub enum CellaGitError {
    /// Git is not installed or not found in PATH.
    #[error("git not found")]
    GitNotFound,

    /// A git command failed.
    #[error("git command failed: {0}")]
    CommandFailed(String),

    /// The current directory is not a git repository.
    #[error("not a git repository")]
    NotARepository,
}
