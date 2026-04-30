//! Git credential handling: host invocation and field formatting.
//!
//! Handles credential requests from the in-container agent via the
//! control socket JSON protocol.

use std::collections::HashMap;
use std::process::Stdio;

pub use cella_protocol::credential::format_credential_fields;

use crate::CellaDaemonError;

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
///
/// # Errors
///
/// Returns error if the git credential command fails or the operation is unknown.
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

    let stdin_data = format_credential_fields(fields);

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

    // ---------------------------------------------------------------
    // format_credential_fields
    // ---------------------------------------------------------------

    #[test]
    fn format_credential_fields_output() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        let output = format_credential_fields(&fields);
        assert!(output.contains("protocol=https\n"));
        assert!(output.ends_with("\n\n"));
    }

    #[test]
    fn format_credential_fields_empty() {
        let fields: HashMap<String, String> = HashMap::new();
        let output = format_credential_fields(&fields);
        assert_eq!(output, "\n");
    }

    #[test]
    fn format_credential_fields_multiple() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        fields.insert("host".to_string(), "github.com".to_string());
        let output = format_credential_fields(&fields);
        // Both fields present
        assert!(output.contains("protocol=https\n"));
        assert!(output.contains("host=github.com\n"));
        // Ends with blank line terminator
        assert!(output.ends_with("\n\n"));
    }

    #[test]
    fn format_credential_fields_empty_value() {
        let mut fields = HashMap::new();
        fields.insert("password".to_string(), String::new());
        let output = format_credential_fields(&fields);
        assert!(output.contains("password=\n"));
    }

    // ---------------------------------------------------------------
    // parse_credential_output
    // ---------------------------------------------------------------

    #[test]
    fn roundtrip_credential_output() {
        let input = "protocol=https\nhost=github.com\nusername=user\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.get("username"), Some(&"user".to_string()));
    }

    #[test]
    fn parse_credential_output_empty_input() {
        let fields = parse_credential_output("");
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_credential_output_blank_lines_ignored() {
        let input = "protocol=https\n\n\nhost=github.com\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields.get("protocol"), Some(&"https".to_string()));
        assert_eq!(fields.get("host"), Some(&"github.com".to_string()));
    }

    #[test]
    fn parse_credential_output_no_equals_ignored() {
        let input = "protocol=https\ngarbage\nhost=github.com\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn parse_credential_output_value_with_equals() {
        let input = "path=/a=b=c\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.get("path"), Some(&"/a=b=c".to_string()));
    }

    #[test]
    fn parse_credential_output_empty_value() {
        let input = "password=\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.get("password"), Some(&String::new()));
    }

    #[test]
    fn parse_credential_output_duplicate_keys_last_wins() {
        let input = "host=first\nhost=second\n";
        let fields = parse_credential_output(input);
        // HashMap behavior: last value wins when collecting
        assert_eq!(fields.get("host"), Some(&"second".to_string()));
    }

    // ---------------------------------------------------------------
    // invoke_git_credential (operation validation only)
    // ---------------------------------------------------------------

    #[test]
    fn unknown_operation_fails() {
        let fields = HashMap::new();
        let result = invoke_git_credential("unknown", &fields);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_operation_error_message() {
        let fields = HashMap::new();
        let err = invoke_git_credential("delete", &fields).unwrap_err();
        assert!(err.to_string().contains("unknown operation: delete"));
    }

    #[test]
    fn empty_operation_fails() {
        let fields = HashMap::new();
        let err = invoke_git_credential("", &fields).unwrap_err();
        assert!(err.to_string().contains("unknown operation"));
    }

    // ---------------------------------------------------------------
    // Roundtrip: format_credential_fields -> parse_credential_output
    // ---------------------------------------------------------------

    #[test]
    fn roundtrip_format_parse() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        fields.insert("host".to_string(), "github.com".to_string());
        let formatted = format_credential_fields(&fields);
        let parsed_back = parse_credential_output(&formatted);
        assert_eq!(parsed_back.get("protocol"), Some(&"https".to_string()));
        assert_eq!(parsed_back.get("host"), Some(&"github.com".to_string()));
    }
}
