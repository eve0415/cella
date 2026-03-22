//! Browser-open handler: opens URLs on the host machine.

use tracing::{info, warn};

/// Handles browser-open requests from in-container agents.
pub struct BrowserHandler {
    /// Command to use for opening URLs.
    open_command: String,
}

impl BrowserHandler {
    /// Create a new browser handler, detecting the appropriate open command.
    pub fn new() -> Self {
        let open_command = detect_open_command();
        Self { open_command }
    }

    /// Open a URL in the host's default browser.
    pub fn open_url(&self, url: &str) {
        info!("Opening URL in browser: {url}");

        let result = std::process::Command::new(&self.open_command)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        match result {
            Ok(_) => info!("Browser opened successfully"),
            Err(e) => warn!("Failed to open browser with '{}': {e}", self.open_command),
        }
    }
}

impl Default for BrowserHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect the appropriate command to open URLs on this platform.
fn detect_open_command() -> String {
    match std::env::consts::OS {
        "macos" => "open".to_string(),
        "windows" => "start".to_string(),
        _ => {
            // Linux: prefer xdg-open
            if which_exists("xdg-open") {
                "xdg-open".to_string()
            } else {
                "open".to_string()
            }
        }
    }
}

/// Check if a command exists in PATH.
fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_open_command_returns_something() {
        let cmd = detect_open_command();
        assert!(!cmd.is_empty());
    }

    #[test]
    fn browser_handler_creates() {
        let handler = BrowserHandler::new();
        assert!(!handler.open_command.is_empty());
    }
}
