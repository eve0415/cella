//! Git credential handling: protocol parsing and host invocation.
//!
//! Migrated from `cella-credential-proxy`. Handles both the legacy
//! wire protocol (direct socket connections) and the new JSON protocol
//! (via control socket from cella-agent).

use std::collections::HashMap;
use std::process::Stdio;

use crate::CellaDaemonError;

/// A parsed credential request (legacy protocol).
#[derive(Debug, Clone)]
pub struct CredentialRequest {
    /// The git credential operation: get, store, erase, or ping.
    pub operation: String,
    /// Key-value fields (protocol, host, username, password, etc.).
    pub fields: HashMap<String, String>,
}

/// A credential response to send back.
#[derive(Debug, Clone)]
pub struct CredentialResponse {
    /// Key-value fields (protocol, host, username, password, etc.).
    pub fields: HashMap<String, String>,
}

/// Parse a credential request from raw text (legacy wire format).
///
/// Format:
/// ```text
/// get\n
/// protocol=https\n
/// host=github.com\n
/// \n
/// ```
pub fn parse_request(data: &str) -> Result<CredentialRequest, CellaDaemonError> {
    let mut lines = data.lines();

    let operation = lines
        .next()
        .ok_or_else(|| CellaDaemonError::Protocol {
            message: "empty request".to_string(),
        })?
        .trim()
        .to_string();

    if operation.is_empty() {
        return Err(CellaDaemonError::Protocol {
            message: "empty operation".to_string(),
        });
    }

    let mut fields = HashMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.to_string(), value.to_string());
        }
    }

    Ok(CredentialRequest { operation, fields })
}

/// Format credential fields for response.
pub fn format_response(response: &CredentialResponse) -> String {
    let mut output = String::new();
    for (key, value) in &response.fields {
        output.push_str(key);
        output.push('=');
        output.push_str(value);
        output.push('\n');
    }
    output.push('\n');
    output
}

/// Format credential fields for piping into `git credential` stdin.
pub fn format_fields_for_stdin<S: std::hash::BuildHasher>(
    fields: &HashMap<String, String, S>,
) -> String {
    let mut output = String::new();
    for (key, value) in fields {
        output.push_str(key);
        output.push('=');
        output.push_str(value);
        output.push('\n');
    }
    output.push('\n');
    output
}

/// Parse credential response from `git credential` stdout.
pub fn parse_credential_output(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Invoke the host's git credential helper for a given operation.
///
/// Translates operations:
/// - `get` -> `git credential fill`
/// - `store` -> `git credential approve`
/// - `erase` -> `git credential reject`
pub fn invoke_git_credential<S: std::hash::BuildHasher>(
    operation: &str,
    fields: &HashMap<String, String, S>,
) -> Result<HashMap<String, String>, CellaDaemonError> {
    let git_op = match operation {
        "get" => "fill",
        "store" => "approve",
        "erase" => "reject",
        other => {
            return Err(CellaDaemonError::GitCredential {
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
        .map_err(|e| CellaDaemonError::GitCredential {
            message: format!("failed to spawn git credential {git_op}: {e}"),
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(stdin_data.as_bytes())
            .map_err(|e| CellaDaemonError::GitCredential {
                message: format!("failed to write to git credential stdin: {e}"),
            })?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| CellaDaemonError::GitCredential {
            message: format!("git credential {git_op} failed: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CellaDaemonError::GitCredential {
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
    fn parse_get_request() {
        let data = "get\nprotocol=https\nhost=github.com\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "get");
        assert_eq!(req.fields.get("protocol"), Some(&"https".to_string()));
    }

    #[test]
    fn parse_empty_request_fails() {
        assert!(parse_request("").is_err());
    }

    #[test]
    fn format_response_output() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        let response = CredentialResponse { fields };
        let output = format_response(&response);
        assert!(output.contains("protocol=https\n"));
        assert!(output.ends_with("\n\n"));
    }

    #[test]
    fn unknown_operation_fails() {
        let fields = HashMap::new();
        let result = invoke_git_credential("unknown", &fields);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_credential_output() {
        let input = "protocol=https\nhost=github.com\nusername=user\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.get("username"), Some(&"user".to_string()));
    }
}
