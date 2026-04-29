//! BROWSER env var handler: sends URLs to the host daemon for opening.

use cella_port::CellaPortError;
use cella_protocol::AgentMessage;

use crate::control::ControlClient;

/// Send a browser-open request to the host daemon.
///
/// Reads connection info from `CELLA_DAEMON_ADDR` / `CELLA_DAEMON_TOKEN`
/// env vars, falling back to the `.daemon_addr` file on the shared volume.
///
/// # Errors
///
/// Returns error if connection info is unavailable or daemon is unreachable.
pub async fn send_browser_open(url: &str) -> Result<(), CellaPortError> {
    let (addr, token) = crate::control::resolve_daemon_connection()?;
    let name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();

    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token).await?;
    let msg = AgentMessage::BrowserOpen {
        url: url.to_string(),
    };
    client.send(&msg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_browser_open_without_daemon_returns_error() {
        if crate::control::resolve_daemon_connection().is_ok() {
            eprintln!("skipping: real daemon connection available, would open browser");
            return;
        }
        let result = send_browser_open("http://localhost:3000").await;
        assert!(result.is_err(), "expected error when daemon is unreachable");
    }
}
