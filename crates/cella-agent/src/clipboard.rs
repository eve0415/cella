use std::io::{Read, Write};

use base64::Engine;
use cella_port::CellaPortError;
use cella_protocol::{AgentMessage, DaemonMessage};
use tracing::{debug, warn};

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
            "-target" | "-t" => {
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

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct WlPasteOptions {
    pub mime_type: String,
    pub list_types: bool,
    pub no_newline: bool,
}

pub fn parse_wl_paste_args(args: &[String]) -> WlPasteOptions {
    let mut mime_type = DEFAULT_MIME_TYPE.to_string();
    let mut list_types = false;
    let mut no_newline = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list-types" | "-l" => list_types = true,
            "-n" | "--no-newline" => no_newline = true,
            "--type" | "-t" => {
                i += 1;
                if let Some(t) = args.get(i) {
                    mime_type.clone_from(t);
                }
            }
            "--seat" => {
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    WlPasteOptions {
        mime_type,
        list_types,
        no_newline,
    }
}

pub fn parse_wl_copy_args(args: &[String]) -> ClipboardOp {
    let mut mime_type = DEFAULT_MIME_TYPE.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clear" | "-c" => return ClipboardOp::Clear,
            "--type" | "-t" => {
                i += 1;
                if let Some(t) = args.get(i) {
                    mime_type.clone_from(t);
                }
            }
            "--seat" => {
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    ClipboardOp::Copy { mime_type }
}

pub async fn handle_wl_paste(args: &[String]) -> Result<(), CellaPortError> {
    let opts = parse_wl_paste_args(args);
    let mime = if opts.list_types {
        "TARGETS".to_string()
    } else {
        opts.mime_type
    };
    let data = request_clipboard_paste(&mime).await?;
    let output = if opts.no_newline {
        data.strip_suffix(b"\n").unwrap_or(&data).to_vec()
    } else {
        data
    };
    std::io::stdout()
        .write_all(&output)
        .map_err(|e| CellaPortError::ControlSocket {
            message: format!("failed to write stdout: {e}"),
        })
}

pub async fn handle_wl_copy(args: &[String]) -> Result<(), CellaPortError> {
    let op = parse_wl_copy_args(args);
    execute_clipboard_op(op, false).await
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
                std::io::stdout()
                    .write_all(&buf)
                    .map_err(|e| CellaPortError::ControlSocket {
                        message: format!("failed to write stdout: {e}"),
                    })?;
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
    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token, true).await?;
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
    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token, true).await?;
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
        warn!("Unexpected daemon response for clipboard paste: {response:?}");
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

    #[test]
    fn parse_xclip_short_target_flag() {
        let op = parse_xclip_args(&["-t".to_string(), "image/png".to_string(), "-o".to_string()]);
        assert!(matches!(op, ClipboardOp::Paste { mime_type } if mime_type == "image/png"));
    }

    #[test]
    fn parse_xclip_short_target_with_targets() {
        let op = parse_xclip_args(&[
            "-selection".to_string(),
            "clipboard".to_string(),
            "-t".to_string(),
            "TARGETS".to_string(),
            "-o".to_string(),
        ]);
        assert!(matches!(op, ClipboardOp::Paste { mime_type } if mime_type == "TARGETS"));
    }

    #[test]
    fn parse_wl_paste_list_types() {
        let opts = parse_wl_paste_args(&["--list-types".to_string()]);
        assert!(opts.list_types);
    }

    #[test]
    fn parse_wl_paste_type_flag() {
        let opts = parse_wl_paste_args(&["--type".to_string(), "image/png".to_string()]);
        assert_eq!(opts.mime_type, "image/png");
        assert!(!opts.list_types);
    }

    #[test]
    fn parse_wl_paste_short_type_flag() {
        let opts = parse_wl_paste_args(&["-t".to_string(), "image/jpeg".to_string()]);
        assert_eq!(opts.mime_type, "image/jpeg");
    }

    #[test]
    fn parse_wl_paste_short_list_flag() {
        let opts = parse_wl_paste_args(&["-l".to_string()]);
        assert!(opts.list_types);
    }

    #[test]
    fn parse_wl_paste_no_newline_flag() {
        let opts = parse_wl_paste_args(&["-n".to_string()]);
        assert!(opts.no_newline);
    }

    #[test]
    fn parse_wl_paste_defaults() {
        let opts = parse_wl_paste_args(&[]);
        assert_eq!(opts.mime_type, "text/plain");
        assert!(!opts.list_types);
        assert!(!opts.no_newline);
    }

    #[test]
    fn parse_wl_copy_defaults_to_copy() {
        let op = parse_wl_copy_args(&[]);
        assert!(matches!(op, ClipboardOp::Copy { mime_type } if mime_type == "text/plain"));
    }

    #[test]
    fn parse_wl_copy_type_flag() {
        let op = parse_wl_copy_args(&["--type".to_string(), "image/png".to_string()]);
        assert!(matches!(op, ClipboardOp::Copy { mime_type } if mime_type == "image/png"));
    }

    #[test]
    fn parse_wl_copy_clear_flag() {
        assert!(matches!(
            parse_wl_copy_args(&["--clear".to_string()]),
            ClipboardOp::Clear
        ));
        assert!(matches!(
            parse_wl_copy_args(&["-c".to_string()]),
            ClipboardOp::Clear
        ));
    }
}
