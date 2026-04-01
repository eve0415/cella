//! BROWSER env var handler: sends URLs to the host daemon for opening.

use cella_port::CellaPortError;
use cella_port::protocol::AgentMessage;

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
    let (addr, token) = resolve_daemon_connection()?;
    let name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();

    let (mut client, _hello) = ControlClient::connect(&addr, &name, &token).await?;
    let msg = AgentMessage::BrowserOpen {
        url: url.to_string(),
    };
    client.send(&msg).await
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
