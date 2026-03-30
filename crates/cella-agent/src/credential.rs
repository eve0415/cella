//! Git credential helper: forwards credential requests to the host daemon.

use std::collections::HashMap;
use std::io::{self, Read};

use cella_port::CellaPortError;
use cella_port::protocol::{AgentMessage, DaemonMessage};

use crate::control::ControlClient;

/// Handle a git credential request by forwarding to the host daemon.
///
/// Reads credential fields from stdin (git credential protocol),
/// sends to daemon, and writes response to stdout.
///
/// Reads connection info from `CELLA_DAEMON_ADDR`, `CELLA_DAEMON_TOKEN`,
/// and `CELLA_CONTAINER_NAME` environment variables.
///
/// # Errors
///
/// Returns error if env vars are missing or control socket communication fails.
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

    let addr = std::env::var("CELLA_DAEMON_ADDR").map_err(|_| CellaPortError::ControlSocket {
        message: "CELLA_DAEMON_ADDR not set".to_string(),
    })?;
    let token = std::env::var("CELLA_DAEMON_TOKEN").map_err(|_| CellaPortError::ControlSocket {
        message: "CELLA_DAEMON_TOKEN not set".to_string(),
    })?;
    let name =
        std::env::var("CELLA_CONTAINER_NAME").map_err(|_| CellaPortError::ControlSocket {
            message: "CELLA_CONTAINER_NAME not set".to_string(),
        })?;

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
}
