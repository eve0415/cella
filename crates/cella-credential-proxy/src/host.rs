//! Invoke host's `git credential fill/approve/reject`.

use std::collections::HashMap;
use std::process::Stdio;

use crate::CellaCredentialProxyError;
use crate::protocol::{format_fields_for_stdin, parse_credential_output};

/// Invoke the host's git credential helper for a given operation.
///
/// Translates operations:
/// - `get` → `git credential fill`
/// - `store` → `git credential approve`
/// - `erase` → `git credential reject`
///
/// Pure pass-through — no caching or credential storage.
///
/// # Errors
///
/// Returns error if git credential invocation fails or returns non-zero.
pub fn invoke_git_credential<S: std::hash::BuildHasher>(
    operation: &str,
    fields: &HashMap<String, String, S>,
) -> Result<HashMap<String, String>, CellaCredentialProxyError> {
    let git_op = match operation {
        "get" => "fill",
        "store" => "approve",
        "erase" => "reject",
        other => {
            return Err(CellaCredentialProxyError::GitCredential {
                message: format!("unknown operation: {other}"),
            });
        }
    };

    let stdin_data = format_fields_for_stdin(fields);

    let mut child = std::process::Command::new("git")
        .args(["credential", git_op])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CellaCredentialProxyError::GitCredential {
            message: format!("failed to spawn git credential {git_op}: {e}"),
        })?;

    // Write credential data to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(stdin_data.as_bytes()).map_err(|e| {
            CellaCredentialProxyError::GitCredential {
                message: format!("failed to write to git credential stdin: {e}"),
            }
        })?;
        // stdin is dropped here, closing the pipe
    }

    let output =
        child
            .wait_with_output()
            .map_err(|e| CellaCredentialProxyError::GitCredential {
                message: format!("git credential {git_op} failed: {e}"),
            })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CellaCredentialProxyError::GitCredential {
            message: format!(
                "git credential {git_op} exited with {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_credential_output(&stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_operation_fails() {
        let fields = HashMap::new();
        let result = invoke_git_credential("unknown", &fields);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown operation"));
    }
}
