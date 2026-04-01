//! Git credential handling: protocol parsing and host invocation.
//!
//! Handles credential requests from the in-container agent via the
//! control socket JSON protocol.

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
///
/// # Errors
///
/// Returns error if the request format is invalid.
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

/// Format credential fields as key=value lines (terminated by a blank line).
///
/// Used both for formatting responses and for piping into `git credential` stdin.
pub fn format_credential_fields<S: std::hash::BuildHasher>(
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
    // parse_request
    // ---------------------------------------------------------------

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
    fn parse_store_request() {
        let data = "store\nprotocol=https\nhost=github.com\nusername=user\npassword=pass\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "store");
        assert_eq!(req.fields.get("username"), Some(&"user".to_string()));
        assert_eq!(req.fields.get("password"), Some(&"pass".to_string()));
        assert_eq!(req.fields.len(), 4);
    }

    #[test]
    fn parse_erase_request() {
        let data = "erase\nhost=gitlab.com\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "erase");
        assert_eq!(req.fields.get("host"), Some(&"gitlab.com".to_string()));
    }

    #[test]
    fn parse_request_whitespace_only_operation_fails() {
        let result = parse_request("   \nhost=github.com\n");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("empty operation"));
    }

    #[test]
    fn parse_request_operation_only_no_fields() {
        let data = "get\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "get");
        assert!(req.fields.is_empty());
    }

    #[test]
    fn parse_request_stops_at_blank_line() {
        let data = "get\nprotocol=https\n\nhost=extra\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.fields.len(), 1);
        assert_eq!(req.fields.get("protocol"), Some(&"https".to_string()));
        // "host=extra" should not be parsed (comes after blank line)
        assert!(!req.fields.contains_key("host"));
    }

    #[test]
    fn parse_request_ignores_lines_without_equals() {
        let data = "get\nnoequals\nprotocol=https\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.fields.len(), 1);
        assert_eq!(req.fields.get("protocol"), Some(&"https".to_string()));
    }

    #[test]
    fn parse_request_trims_operation_whitespace() {
        let data = "  get  \nprotocol=https\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "get");
    }

    #[test]
    fn parse_request_trims_field_whitespace() {
        let data = "get\n  protocol=https  \n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.fields.get("protocol"), Some(&"https".to_string()));
    }

    #[test]
    fn parse_request_value_with_equals_sign() {
        // Values can contain '=' (split_once splits only at first '=')
        let data = "get\npath=/a=b\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.fields.get("path"), Some(&"/a=b".to_string()));
    }

    #[test]
    fn parse_request_operation_only_no_newline() {
        let data = "ping";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "ping");
        assert!(req.fields.is_empty());
    }

    #[test]
    fn parse_request_multiple_fields() {
        let data = "get\nprotocol=https\nhost=github.com\npath=/repo.git\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.fields.len(), 3);
        assert_eq!(req.fields.get("path"), Some(&"/repo.git".to_string()));
    }

    #[test]
    fn parse_request_empty_value() {
        let data = "get\npassword=\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.fields.get("password"), Some(&String::new()));
    }

    #[test]
    fn parse_request_error_display() {
        let err = parse_request("").unwrap_err();
        assert!(err.to_string().contains("protocol error"));
    }

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
    // CredentialRequest / CredentialResponse struct construction
    // ---------------------------------------------------------------

    #[test]
    fn credential_request_debug() {
        let req = CredentialRequest {
            operation: "get".to_string(),
            fields: HashMap::new(),
        };
        let debug = format!("{req:?}");
        assert!(debug.contains("get"));
    }

    #[test]
    fn credential_request_clone() {
        let mut fields = HashMap::new();
        fields.insert("host".to_string(), "github.com".to_string());
        let req = CredentialRequest {
            operation: "get".to_string(),
            fields,
        };
        #[allow(clippy::redundant_clone)]
        let cloned = req.clone();
        assert_eq!(cloned.operation, "get");
        assert_eq!(cloned.fields.get("host"), Some(&"github.com".to_string()));
    }

    #[test]
    fn credential_response_debug_and_clone() {
        let mut fields = HashMap::new();
        fields.insert("username".to_string(), "user".to_string());
        let resp = CredentialResponse { fields };
        let cloned = resp.clone();
        assert_eq!(cloned.fields.get("username"), Some(&"user".to_string()));
        let debug = format!("{resp:?}");
        assert!(debug.contains("username"));
    }

    // ---------------------------------------------------------------
    // Roundtrip: parse_request -> format_credential_fields -> parse_credential_output
    // ---------------------------------------------------------------

    #[test]
    fn roundtrip_parse_format_parse() {
        let data = "get\nprotocol=https\nhost=github.com\n\n";
        let req = parse_request(data).unwrap();
        let formatted = format_credential_fields(&req.fields);
        let parsed_back = parse_credential_output(&formatted);
        assert_eq!(parsed_back.get("protocol"), Some(&"https".to_string()));
        assert_eq!(parsed_back.get("host"), Some(&"github.com".to_string()));
    }
}
