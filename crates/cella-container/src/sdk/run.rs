//! Command execution helpers for the Apple `container` CLI.

use std::path::Path;
use std::process::Stdio;

use cella_backend::BackendError;
use tokio::process::Command;
use tracing::debug;

/// Captured output from a CLI invocation.
#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Run the `container` CLI binary with the given arguments and capture output.
///
/// # Errors
///
/// Returns `BackendError::HostCommandFailed` if the binary cannot be spawned.
pub async fn run_cli(binary: &Path, args: &[&str]) -> Result<CommandOutput, BackendError> {
    debug!(binary = %binary.display(), ?args, "running container CLI");

    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| BackendError::HostCommandFailed {
            command: format!("{} {}", binary.display(), args.join(" ")),
            source: e,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    debug!(
        exit_code,
        stdout_len = stdout.len(),
        stderr_len = stderr.len(),
        "CLI output"
    );

    Ok(CommandOutput {
        stdout,
        stderr,
        exit_code,
    })
}

/// Run the CLI with the given `String` args (owned version of [`run_cli`]).
///
/// # Errors
///
/// Returns `BackendError::HostCommandFailed` if the binary cannot be spawned.
pub async fn run_cli_owned(binary: &Path, args: &[String]) -> Result<CommandOutput, BackendError> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cli(binary, &refs).await
}

/// Run the CLI and parse the JSON stdout into `T`.
///
/// # Errors
///
/// Returns an error if the CLI exits non-zero, cannot be spawned, or the
/// output is not valid JSON for `T`.
pub async fn run_cli_json<T: serde::de::DeserializeOwned>(
    binary: &Path,
    args: &[&str],
) -> Result<T, BackendError> {
    let output = run_cli(binary, args).await?;
    if output.exit_code != 0 {
        return Err(BackendError::Runtime(output.stderr.into()));
    }
    serde_json::from_str(&output.stdout).map_err(|e| BackendError::Runtime(Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_cli_missing_binary() {
        let result = run_cli(Path::new("/nonexistent/binary"), &["version"]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, BackendError::HostCommandFailed { .. }),
            "expected HostCommandFailed, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn run_cli_captures_output() {
        // Use `echo` as a trivially available binary.
        let result = run_cli(Path::new("/bin/echo"), &["hello", "world"]).await;
        let output = result.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "hello world");
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_cli_json_parses_output() {
        // Use printf via sh to produce valid JSON.
        let result: Result<Vec<String>, _> =
            run_cli_json(Path::new("/bin/sh"), &["-c", r#"echo '["a","b"]'"#]).await;
        let parsed = result.unwrap();
        assert_eq!(parsed, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn run_cli_json_error_on_nonzero_exit() {
        let result: Result<serde_json::Value, _> =
            run_cli_json(Path::new("/bin/sh"), &["-c", "echo 'boom' >&2; exit 1"]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_cli_json_error_on_invalid_json() {
        let result: Result<serde_json::Value, _> =
            run_cli_json(Path::new("/bin/sh"), &["-c", "echo 'not json'"]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_cli_owned_works() {
        let args = vec!["hello".to_string(), "world".to_string()];
        let result = run_cli_owned(Path::new("/bin/echo"), &args).await;
        let output = result.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "hello world");
    }
}
