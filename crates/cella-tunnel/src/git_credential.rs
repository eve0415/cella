//! Git credential channel handler.
//!
//! Buffers incoming credential request data until the terminator (`\n\n`),
//! then invokes the host `git credential` command and sends the response
//! back over the mux channel.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::mux::{close_frame, data_frame, write_frame_async};

/// Handle a credential channel.
///
/// Buffers incoming data, parses the git credential protocol request,
/// invokes `git credential` on the host, and sends the response.
pub async fn handle_channel(
    channel_id: u32,
    mut rx: mpsc::Receiver<Vec<u8>>,
    exec_writer: Arc<Mutex<ChildStdin>>,
) {
    // Buffer incoming data until we have a complete request (\n\n terminated)
    let mut buffer = Vec::new();
    while let Some(payload) = rx.recv().await {
        buffer.extend_from_slice(&payload);
        if buffer.ends_with(b"\n\n") {
            break;
        }
    }

    if buffer.is_empty() {
        let mut w = exec_writer.lock().await;
        let _ = write_frame_async(&mut *w, &close_frame(channel_id)).await;
        return;
    }

    // Parse and invoke git credential (blocking)
    let request_str = String::from_utf8_lossy(&buffer).to_string();
    let response = tokio::task::spawn_blocking(move || invoke_credential(&request_str))
        .await
        .unwrap_or_default();

    // Send response back
    if !response.is_empty() {
        let mut w = exec_writer.lock().await;
        let _ = write_frame_async(&mut *w, &data_frame(channel_id, response.into_bytes())).await;
    }

    // Send CLOSE
    let mut w = exec_writer.lock().await;
    let _ = write_frame_async(&mut *w, &close_frame(channel_id)).await;
}

/// Parse a credential request and invoke `git credential` on the host.
///
/// Request format: `operation\nkey=value\n...\n\n`
fn invoke_credential(request: &str) -> String {
    let mut lines = request.lines();
    let operation = match lines.next() {
        Some(op) if !op.is_empty() => op.trim(),
        _ => return String::new(),
    };

    let git_op = match operation {
        "get" => "fill",
        "store" => "approve",
        "erase" => "reject",
        _ => {
            warn!("Unknown credential operation: {operation}");
            return String::new();
        }
    };

    // Collect fields for git credential stdin
    let mut fields_str = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        fields_str.push_str(line);
        fields_str.push('\n');
    }
    fields_str.push('\n');

    let mut child = match std::process::Command::new("git")
        .args(["credential", git_op])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to spawn git credential {git_op}: {e}");
            return String::new();
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(fields_str.as_bytes());
    }

    let output = match child.wait_with_output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            warn!(
                "git credential {git_op} exited with {}: {}",
                o.status.code().unwrap_or(-1),
                stderr.trim()
            );
            return String::new();
        }
        Err(e) => {
            warn!("git credential {git_op} failed: {e}");
            return String::new();
        }
    };

    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Parse credential response from `git credential` stdout.
#[allow(dead_code)]
fn parse_credential_output(output: &str) -> HashMap<String, String> {
    output
        .lines()
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
    fn unknown_operation_returns_empty() {
        let result = invoke_credential("unknown\nprotocol=https\n\n");
        assert!(result.is_empty());
    }

    #[test]
    fn empty_request_returns_empty() {
        let result = invoke_credential("");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_credential_output_roundtrip() {
        let input = "protocol=https\nhost=github.com\nusername=user\npassword=ghp_xxx\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.get("username"), Some(&"user".to_string()));
        assert_eq!(fields.get("password"), Some(&"ghp_xxx".to_string()));
    }
}
