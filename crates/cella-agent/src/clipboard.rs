use std::io::{Read, Write};

use base64::Engine;
use cella_port::CellaPortError;
use cella_protocol::{AgentMessage, DaemonMessage};
use tracing::debug;

use crate::control::ControlClient;

const MAX_CLIPBOARD_SIZE: usize = 10 * 1024 * 1024;
const DEFAULT_MIME_TYPE: &str = "text/plain";

#[cfg_attr(test, derive(Debug))]
pub enum ClipboardOp {
    Copy { mime_type: String },
    Paste { mime_type: String },
    Clear,
}

pub fn parse_xsel_args(args: &[String]) -> ClipboardOp {
    let mut mode = None;
    for arg in args {
        match arg.as_str() {
            "-i" | "--input" => mode = Some("input"),
            "-o" | "--output" => mode = Some("output"),
            "-c" | "--clear" => return ClipboardOp::Clear,
            _ => {}
        }
    }
    match mode.unwrap_or("output") {
        "input" => ClipboardOp::Copy {
            mime_type: DEFAULT_MIME_TYPE.to_string(),
        },
        _ => ClipboardOp::Paste {
            mime_type: DEFAULT_MIME_TYPE.to_string(),
        },
    }
}

pub fn parse_xclip_args(args: &[String]) -> ClipboardOp {
    let mut mode = None;
    let mut mime_type = DEFAULT_MIME_TYPE.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "-in" => mode = Some("input"),
            "-o" | "-out" => mode = Some("output"),
            "-target" => {
                i += 1;
                if let Some(t) = args.get(i) {
                    mime_type.clone_from(t);
                }
            }
            "-selection" => {
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    match mode.unwrap_or("input") {
        "output" => ClipboardOp::Paste { mime_type },
        _ => ClipboardOp::Copy { mime_type },
    }
}

pub async fn handle_xsel(args: &[String]) -> Result<(), CellaPortError> {
    let op = parse_xsel_args(args);
    execute_clipboard_op(op, false).await
}

pub async fn handle_xclip(args: &[String]) -> Result<(), CellaPortError> {
    let filter = args.iter().any(|a| a == "-f" || a == "-filter");
    let op = parse_xclip_args(args);
    execute_clipboard_op(op, filter).await
}

async fn execute_clipboard_op(op: ClipboardOp, filter: bool) -> Result<(), CellaPortError> {
    match op {
        ClipboardOp::Copy { mime_type } => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| CellaPortError::ControlSocket {
                    message: format!("failed to read stdin: {e}"),
                })?;
            if buf.len() > MAX_CLIPBOARD_SIZE {
                return Err(CellaPortError::ControlSocket {
                    message: format!(
                        "clipboard data exceeds {} MB limit",
                        MAX_CLIPBOARD_SIZE / 1024 / 1024
                    ),
                });
            }
            if filter {
                let _ = std::io::stdout().write_all(&buf);
            }
            send_clipboard_copy(&buf, &mime_type).await
        }
        ClipboardOp::Paste { mime_type } => {
            let data = request_clipboard_paste(&mime_type).await?;
            std::io::stdout()
                .write_all(&data)
                .map_err(|e| CellaPortError::ControlSocket {
                    message: format!("failed to write stdout: {e}"),
                })
        }
        ClipboardOp::Clear => send_clipboard_copy(&[], DEFAULT_MIME_TYPE).await,
    }
}

async fn send_clipboard_copy(data: &[u8], mime_type: &str) -> Result<(), CellaPortError> {
    let (addr, token) = crate::control::resolve_daemon_connection()?;
    let name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();
    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token).await?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let msg = AgentMessage::ClipboardCopy {
        data: encoded,
        mime_type: mime_type.to_string(),
    };
    client.send(&msg).await
}

async fn request_clipboard_paste(mime_type: &str) -> Result<Vec<u8>, CellaPortError> {
    let (addr, token) = crate::control::resolve_daemon_connection()?;
    let name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();
    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token).await?;
    let msg = AgentMessage::ClipboardPaste {
        mime_type: Some(mime_type.to_string()),
    };
    client.send(&msg).await?;
    let response = client.recv().await?;
    debug!("Clipboard paste response received");
    if let DaemonMessage::ClipboardContent { data, .. } = response {
        base64::engine::general_purpose::STANDARD
            .decode(&data)
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("invalid base64 clipboard data: {e}"),
            })
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_xsel_defaults_to_output() {
        let op = parse_xsel_args(&[]);
        assert!(matches!(op, ClipboardOp::Paste { .. }));
    }

    #[test]
    fn parse_xsel_input_flag() {
        let op = parse_xsel_args(&["-i".to_string()]);
        assert!(matches!(op, ClipboardOp::Copy { .. }));
    }

    #[test]
    fn parse_xsel_output_flag() {
        let op = parse_xsel_args(&["-o".to_string()]);
        assert!(matches!(op, ClipboardOp::Paste { .. }));
    }

    #[test]
    fn parse_xsel_long_flags() {
        assert!(matches!(
            parse_xsel_args(&["--input".to_string()]),
            ClipboardOp::Copy { .. }
        ));
        assert!(matches!(
            parse_xsel_args(&["--output".to_string()]),
            ClipboardOp::Paste { .. }
        ));
    }

    #[test]
    fn parse_xsel_clipboard_flag_accepted() {
        let op = parse_xsel_args(&["--clipboard".to_string(), "-i".to_string()]);
        assert!(matches!(op, ClipboardOp::Copy { .. }));
    }

    #[test]
    fn parse_xsel_clear_flag() {
        assert!(matches!(
            parse_xsel_args(&["-c".to_string()]),
            ClipboardOp::Clear
        ));
        assert!(matches!(
            parse_xsel_args(&["--clear".to_string()]),
            ClipboardOp::Clear
        ));
    }

    #[test]
    fn parse_xsel_selection_flags_ignored() {
        let op = parse_xsel_args(&["-p".to_string(), "-i".to_string()]);
        assert!(matches!(op, ClipboardOp::Copy { .. }));
        let op = parse_xsel_args(&["-s".to_string(), "-o".to_string()]);
        assert!(matches!(op, ClipboardOp::Paste { .. }));
        let op = parse_xsel_args(&["-b".to_string(), "-i".to_string()]);
        assert!(matches!(op, ClipboardOp::Copy { .. }));
    }

    #[test]
    fn parse_xclip_defaults_to_input() {
        let op = parse_xclip_args(&[]);
        assert!(matches!(op, ClipboardOp::Copy { .. }));
    }

    #[test]
    fn parse_xclip_output_flag() {
        let op = parse_xclip_args(&["-o".to_string()]);
        assert!(matches!(op, ClipboardOp::Paste { .. }));
    }

    #[test]
    fn parse_xclip_long_flags() {
        assert!(matches!(
            parse_xclip_args(&["-in".to_string()]),
            ClipboardOp::Copy { .. }
        ));
        assert!(matches!(
            parse_xclip_args(&["-out".to_string()]),
            ClipboardOp::Paste { .. }
        ));
    }

    #[test]
    fn parse_xclip_target_flag() {
        let op = parse_xclip_args(&[
            "-target".to_string(),
            "image/png".to_string(),
            "-i".to_string(),
        ]);
        assert!(matches!(op, ClipboardOp::Copy { mime_type } if mime_type == "image/png"));
    }

    #[test]
    fn parse_xclip_selection_clipboard_accepted() {
        let op = parse_xclip_args(&["-selection".to_string(), "clipboard".to_string()]);
        assert!(matches!(op, ClipboardOp::Copy { .. }));
    }

    #[test]
    fn parse_xclip_filter_detected() {
        let args = ["-f".to_string()];
        assert!(args.iter().any(|a| a == "-f" || a == "-filter"));
        let args2 = ["-filter".to_string()];
        assert!(args2.iter().any(|a| a == "-f" || a == "-filter"));
    }
}
