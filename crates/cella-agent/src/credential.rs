//! Git credential helper: forwards credential requests to the host daemon.

use std::collections::HashMap;
use std::io::{self, Read};

use cella_port::CellaPortError;
use cella_protocol::{AgentMessage, DaemonMessage};

use crate::control::ControlClient;

/// Handle a git credential request by forwarding to the host daemon.
///
/// Reads credential fields from stdin (git credential protocol),
/// sends to daemon, and writes response to stdout.
///
/// Reads connection info from `CELLA_DAEMON_ADDR` / `CELLA_DAEMON_TOKEN`
/// env vars, falling back to the `.daemon_addr` file on the shared volume.
///
/// # Errors
///
/// Returns error if connection info is unavailable or control socket communication fails.
pub async fn handle_credential(operation: &str) -> Result<(), CellaPortError> {
    // Read credential fields from stdin
    let mut stdin_data = String::new();
    io::stdin()
        .read_to_string(&mut stdin_data)
        .map_err(|e| CellaPortError::ControlSocket {
            message: format!("failed to read stdin: {e}"),
        })?;

    let fields = parse_credential_fields(&stdin_data);

    let request_id = format!(
        "cred-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let (addr, token) = resolve_daemon_connection()?;
    let name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();

    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token).await?;

    let msg = AgentMessage::CredentialRequest {
        id: request_id.clone(),
        operation: operation.to_string(),
        fields,
    };
    client.send(&msg).await?;

    // For "get" operations, wait for response and print to stdout
    if operation == "get" {
        let response = client.recv().await?;
        if let DaemonMessage::CredentialResponse { fields, .. } = response {
            for (key, value) in &fields {
                println!("{key}={value}");
            }
        }
    }

    Ok(())
}

/// Resolve daemon connection info: `.daemon_addr` file first (authoritative),
/// env vars as fallback (may be stale after container restart).
fn resolve_daemon_connection() -> Result<(String, String), CellaPortError> {
    if let Some(info) = crate::control::read_daemon_addr_file() {
        return Ok((info.addr, info.token));
    }
    if let (Ok(addr), Ok(token)) = (
        std::env::var("CELLA_DAEMON_ADDR"),
        std::env::var("CELLA_DAEMON_TOKEN"),
    ) {
        return Ok((addr, token));
    }
    Err(CellaPortError::ControlSocket {
        message: "no daemon connection info available (env vars not set, .daemon_addr not found)"
            .to_string(),
    })
}

/// Parse git credential protocol fields from stdin.
fn parse_credential_fields(data: &str) -> HashMap<String, String> {
    data.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fields() {
        let input = "protocol=https\nhost=github.com\n\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("protocol"), Some(&"https".to_string()));
        assert_eq!(fields.get("host"), Some(&"github.com".to_string()));
    }

    #[test]
    fn parse_empty() {
        let fields = parse_credential_fields("");
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_with_password() {
        let input = "protocol=https\nhost=github.com\nusername=user\npassword=secret\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("protocol"), Some(&"https".to_string()));
        assert_eq!(fields.get("host"), Some(&"github.com".to_string()));
        assert_eq!(fields.get("username"), Some(&"user".to_string()));
        assert_eq!(fields.get("password"), Some(&"secret".to_string()));
    }

    #[test]
    fn parse_lines_without_equals_are_skipped() {
        let input = "protocol=https\nbadline\nhost=github.com\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.len(), 2);
        assert!(fields.contains_key("protocol"));
        assert!(fields.contains_key("host"));
    }

    #[test]
    fn parse_value_with_equals_sign() {
        // Git credential protocol allows values with = (e.g., oauth tokens).
        let input = "password=abc=def=ghi\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("password"), Some(&"abc=def=ghi".to_string()));
    }

    #[test]
    fn parse_trailing_newlines_and_blanks() {
        let input = "protocol=https\n\n\nhost=github.com\n\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn parse_only_blank_lines() {
        let input = "\n\n\n";
        let fields = parse_credential_fields(input);
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_single_field() {
        let input = "host=example.com";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields.get("host"), Some(&"example.com".to_string()));
    }

    #[test]
    fn resolve_daemon_connection_returns_result() {
        let result = resolve_daemon_connection();
        match result {
            Ok((addr, token)) => {
                assert!(!addr.is_empty());
                assert!(!token.is_empty());
            }
            Err(err) => {
                let msg = err.to_string();
                assert!(!msg.is_empty());
            }
        }
    }

    #[test]
    fn parse_credential_fields_duplicate_keys_last_wins() {
        let input = "host=first.com\nhost=second.com\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("host"), Some(&"second.com".to_string()));
    }

    #[test]
    fn parse_credential_fields_empty_value() {
        let input = "host=\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("host"), Some(&String::new()));
    }

    #[test]
    fn parse_credential_fields_windows_line_endings() {
        let input = "protocol=https\r\nhost=github.com\r\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.len(), 2);
        assert!(fields.contains_key("protocol"));
        assert!(fields.contains_key("host"));
    }

    #[test]
    fn parse_credential_fields_many_fields() {
        let input =
            "protocol=https\nhost=github.com\nusername=bot\npassword=token123\npath=org/repo\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.len(), 5);
        assert_eq!(fields.get("path"), Some(&"org/repo".to_string()));
    }

    #[test]
    fn parse_credential_fields_key_only_no_equals() {
        let input = "noequals\n";
        let fields = parse_credential_fields(input);
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_credential_fields_equals_in_key_position() {
        let input = "=value\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get(""), Some(&"value".to_string()));
    }
}
