use std::sync::Arc;

use tracing::{debug, info, warn};

pub struct ClipboardHandler {
    backend: Arc<dyn ClipboardBackend>,
}

trait ClipboardBackend: Send + Sync {
    fn name(&self) -> &str;
    fn copy(&self, data: &[u8], mime_type: &str) -> Result<(), String>;
    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String>;
}

impl ClipboardHandler {
    pub fn new() -> Self {
        let backend = detect_backend();
        info!("Clipboard backend: {}", backend.name());
        Self { backend }
    }

    pub fn null() -> Self {
        Self {
            backend: Arc::new(NullBackend),
        }
    }

    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    pub async fn copy(&self, data: &[u8], mime_type: &str) -> Result<(), String> {
        let backend = self.backend.clone();
        let data = data.to_vec();
        let mime = mime_type.to_string();
        tokio::task::spawn_blocking(move || backend.copy(&data, &mime))
            .await
            .map_err(|e| format!("clipboard task join: {e}"))?
    }

    pub async fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        let backend = self.backend.clone();
        let mime = mime_type.to_string();
        tokio::task::spawn_blocking(move || backend.paste(&mime))
            .await
            .map_err(|e| format!("clipboard task join: {e}"))?
    }
}

fn detect_backend() -> Arc<dyn ClipboardBackend> {
    if cfg!(target_os = "macos") && which_exists("pbcopy") {
        return Arc::new(PbcopyBackend);
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() && which_exists("wl-copy") {
        return Arc::new(WlClipboardBackend);
    }
    if std::env::var("DISPLAY").is_ok() && which_exists("xsel") {
        return Arc::new(XselBackend);
    }
    if std::env::var("DISPLAY").is_ok() && which_exists("xclip") {
        return Arc::new(XclipBackend);
    }
    // OSC 52 only works when the daemon runs in a foreground terminal (rare).
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Arc::new(Osc52Backend);
    }
    warn!("No clipboard backend available — clipboard operations will be no-ops");
    Arc::new(NullBackend)
}

fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// --- Backends ---

struct NullBackend;

impl ClipboardBackend for NullBackend {
    fn name(&self) -> &str {
        "null"
    }

    fn copy(&self, _data: &[u8], _mime_type: &str) -> Result<(), String> {
        Ok(())
    }

    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        Ok((Vec::new(), mime_type.to_string()))
    }
}

struct PbcopyBackend;

impl ClipboardBackend for PbcopyBackend {
    fn name(&self) -> &str {
        "pbcopy"
    }

    fn copy(&self, data: &[u8], _mime_type: &str) -> Result<(), String> {
        pipe_to_command("pbcopy", &[], data)
    }

    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        let output = run_command("pbpaste", &[])?;
        Ok((output, mime_type.to_string()))
    }
}

struct WlClipboardBackend;

impl ClipboardBackend for WlClipboardBackend {
    fn name(&self) -> &str {
        "wl-clipboard"
    }

    fn copy(&self, data: &[u8], mime_type: &str) -> Result<(), String> {
        pipe_to_command("wl-copy", &["--type", mime_type], data)
    }

    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        let output = run_command("wl-paste", &["--type", mime_type])?;
        Ok((output, mime_type.to_string()))
    }
}

struct XselBackend;

impl ClipboardBackend for XselBackend {
    fn name(&self) -> &str {
        "xsel"
    }

    fn copy(&self, data: &[u8], _mime_type: &str) -> Result<(), String> {
        pipe_to_command("xsel", &["--clipboard", "--input"], data)
    }

    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        let output = run_command("xsel", &["--clipboard", "--output"])?;
        Ok((output, mime_type.to_string()))
    }
}

struct XclipBackend;

impl ClipboardBackend for XclipBackend {
    fn name(&self) -> &str {
        "xclip"
    }

    fn copy(&self, data: &[u8], mime_type: &str) -> Result<(), String> {
        pipe_to_command(
            "xclip",
            &["-selection", "clipboard", "-target", mime_type],
            data,
        )
    }

    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        let output = run_command(
            "xclip",
            &["-selection", "clipboard", "-target", mime_type, "-o"],
        )?;
        Ok((output, mime_type.to_string()))
    }
}

struct Osc52Backend;

impl ClipboardBackend for Osc52Backend {
    fn name(&self) -> &str {
        "osc52"
    }

    fn copy(&self, data: &[u8], _mime_type: &str) -> Result<(), String> {
        use base64::Engine;
        use std::io::Write;
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        let seq = format!("\x1b]52;c;{encoded}\x07");
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(seq.as_bytes())
            .map_err(|e| format!("osc52: {e}"))?;
        stdout.flush().map_err(|e| format!("osc52 flush: {e}"))
    }

    fn paste(&self, mime_type: &str) -> Result<(Vec<u8>, String), String> {
        debug!("OSC 52 paste not supported, returning empty");
        Ok((Vec::new(), mime_type.to_string()))
    }
}

fn pipe_to_command(cmd: &str, args: &[&str], data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(data)
        .map_err(|e| format!("{cmd} stdin: {e}"))?;
    let status = child.wait().map_err(|e| format!("{cmd} wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}

fn run_command(cmd: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!("{cmd} exited with {}", output.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_backend_copy_succeeds() {
        let handler = ClipboardHandler::null();
        let result = handler.copy(b"hello", "text/plain").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn null_backend_paste_returns_empty() {
        let handler = ClipboardHandler::null();
        let (data, mime) = handler.paste("text/plain").await.unwrap();
        assert!(data.is_empty());
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn detect_clipboard_command_returns_something() {
        let handler = ClipboardHandler::new();
        assert!(!handler.backend_name().is_empty());
    }

    #[test]
    fn null_backend_name() {
        let handler = ClipboardHandler::null();
        assert_eq!(handler.backend_name(), "null");
    }

    #[tokio::test]
    async fn null_backend_copy_empty_data() {
        let handler = ClipboardHandler::null();
        assert!(handler.copy(b"", "text/plain").await.is_ok());
    }

    #[tokio::test]
    async fn null_backend_paste_preserves_mime() {
        let handler = ClipboardHandler::null();
        let (_, mime) = handler.paste("image/png").await.unwrap();
        assert_eq!(mime, "image/png");
    }
}
