//! BROWSER env var handler: sends URLs to the host daemon for opening.

use cella_port::CellaPortError;
use cella_port::protocol::AgentMessage;

use crate::control::ControlClient;

/// Send a browser-open request to the host daemon.
///
/// Reads connection info from `CELLA_DAEMON_ADDR`, `CELLA_DAEMON_TOKEN`,
/// and `CELLA_CONTAINER_NAME` environment variables.
///
/// # Errors
///
/// Returns error if env vars are missing or daemon is unreachable.
pub async fn send_browser_open(url: &str) -> Result<(), CellaPortError> {
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

    let mut client = ControlClient::connect(&addr, &name, &token).await?;
    let msg = AgentMessage::BrowserOpen {
        url: url.to_string(),
    };
    client.send(&msg).await
}
