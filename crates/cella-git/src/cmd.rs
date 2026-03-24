//! Git command runner with retry on lock contention.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use crate::CellaGitError;

const MAX_RETRIES: u32 = 4;
const BASE_DELAY_MS: u64 = 100;

/// Verify git is installed. Cached via `OnceLock`.
fn git_binary() -> Result<&'static str, CellaGitError> {
    static BINARY: OnceLock<Result<&'static str, ()>> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            Command::new("git")
                .arg("--version")
                .output()
                .map_or(Err(()), |o| {
                    if o.status.success() {
                        Ok("git")
                    } else {
                        Err(())
                    }
                })
        })
        .as_ref()
        .copied()
        .map_err(|()| CellaGitError::GitNotFound)
}

/// Run a git command in the given directory. Returns stdout on success.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String, CellaGitError> {
    let bin = git_binary()?;
    run_with_retry(bin, cwd, args)
}

/// Check whether a git stderr message indicates lock contention.
fn is_lock_error(stderr: &str) -> bool {
    stderr.contains("Unable to create") && stderr.contains(".lock")
}

/// Run with exponential backoff retry on lock contention.
fn run_with_retry(bin: &str, cwd: &Path, args: &[&str]) -> Result<String, CellaGitError> {
    for attempt in 0..=MAX_RETRIES {
        let output = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(CellaGitError::Io)?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_lock_error(&stderr) && attempt < MAX_RETRIES {
            let delay = Duration::from_millis(BASE_DELAY_MS * 2u64.pow(attempt));
            warn!(
                "git lock contention (attempt {}/{}), retrying in {delay:?}",
                attempt + 1,
                MAX_RETRIES
            );
            thread::sleep(delay);
            continue;
        }

        debug!("git {} failed: {stderr}", args.join(" "));
        return Err(CellaGitError::CommandFailed {
            args: args.join(" "),
            stderr: stderr.trim().to_string(),
        });
    }

    Err(CellaGitError::LockContention {
        path: cwd.to_path_buf(),
    })
}

/// Run a git command and return true if exit status is 0.
/// Does not treat non-zero as an error — useful for `--verify --quiet` checks.
pub fn run_quiet(cwd: &Path, args: &[&str]) -> Result<bool, CellaGitError> {
    let bin = git_binary()?;
    let output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(CellaGitError::Io)?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_error_detected() {
        let stderr = "fatal: Unable to create '/tmp/repo/.git/index.lock': File exists.";
        assert!(is_lock_error(stderr));
    }

    #[test]
    fn lock_error_not_triggered_by_unrelated() {
        let stderr = "fatal: not a git repository";
        assert!(!is_lock_error(stderr));
    }

    #[test]
    fn lock_error_requires_both_markers() {
        assert!(!is_lock_error("Unable to create something"));
        assert!(!is_lock_error("file.lock not found"));
    }

    #[test]
    fn run_in_valid_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let result = run(tmp.path(), &["status", "--porcelain"]);
        assert!(result.is_ok());
    }

    #[test]
    fn run_with_invalid_args() {
        let tmp = tempfile::TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let result = run(tmp.path(), &["not-a-real-command"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            CellaGitError::CommandFailed { args, stderr } => {
                assert_eq!(args, "not-a-real-command");
                assert!(!stderr.is_empty());
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn run_quiet_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let result = run_quiet(tmp.path(), &["rev-parse", "--is-inside-work-tree"]);
        assert!(result.unwrap());
    }

    #[test]
    fn run_quiet_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let result = run_quiet(
            tmp.path(),
            &["show-ref", "--verify", "--quiet", "refs/heads/nonexistent"],
        );
        assert!(!result.unwrap());
    }

    #[test]
    fn run_returns_stdout() {
        let tmp = tempfile::TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let result = run(tmp.path(), &["rev-parse", "--is-inside-work-tree"]).unwrap();
        assert_eq!(result.trim(), "true");
    }

    #[test]
    fn run_in_nonexistent_dir() {
        let result = run(std::path::Path::new("/nonexistent-dir-xyz"), &["status"]);
        assert!(result.is_err());
    }

    #[test]
    fn lock_error_with_full_message() {
        let stderr = "fatal: Unable to create '/home/user/repo/.git/refs/heads/main.lock': File exists.\n\
                       Another git process seems to be running in this repository";
        assert!(is_lock_error(stderr));
    }
}
