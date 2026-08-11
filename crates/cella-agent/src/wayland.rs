//! Bridges the Wayland clipboard socket to the daemon control channel.
//!
//! `cella-wayland` owns the protocol and knows nothing about cella; this module
//! is the other half — it implements `ClipboardSource` against the same
//! `ClipboardCopy`/`ClipboardPaste` messages the `/cella/bin` shims use, so both
//! front ends converge on one back end.

use std::path::Path;
use std::sync::Arc;

use cella_wayland::{
    ClipboardSource, MAX_CLIPBOARD_SIZE, ServerHandle, SourceError, WaylandClipboardServer,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::clipboard::{request_clipboard_paste, send_clipboard_copy};

/// The sentinel the daemon echoes back as the first line of a format list.
const TARGETS_SENTINEL: &str = "TARGETS";

/// `ClipboardSource` backed by the daemon control channel.
///
/// The Wayland server runs on a dedicated `std::thread`, never a tokio worker,
/// so `Handle::block_on` is safe here — it panics only when called from inside
/// a runtime thread.
pub struct DaemonClipboardSource {
    handle: Handle,
}

impl DaemonClipboardSource {
    pub const fn new(handle: Handle) -> Self {
        Self { handle }
    }
}

impl ClipboardSource for DaemonClipboardSource {
    fn targets(&self) -> Result<Vec<String>, SourceError> {
        let raw = self
            .handle
            .block_on(request_clipboard_paste(TARGETS_SENTINEL))
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        Ok(parse_targets_response(&raw))
    }

    fn fetch(&self, mime_type: &str) -> Result<Vec<u8>, SourceError> {
        self.handle
            .block_on(request_clipboard_paste(mime_type))
            .map_err(|e| SourceError::Unavailable(e.to_string()))
    }

    fn publish(&self, mime_type: &str, data: &[u8]) -> Result<(), SourceError> {
        if data.len() > MAX_CLIPBOARD_SIZE {
            return Err(SourceError::TooLarge {
                actual: data.len(),
                limit: MAX_CLIPBOARD_SIZE,
            });
        }
        self.handle
            .block_on(send_clipboard_copy(data, mime_type))
            .map_err(|e| SourceError::Unavailable(e.to_string()))
    }
}

/// Turns the daemon's newline-separated format list into mime types.
///
/// The daemon echoes the `TARGETS` sentinel back as the first entry; Wayland
/// clients must never see it as an offerable type.
fn parse_targets_response(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != TARGETS_SENTINEL)
        .map(ToString::to_string)
        .collect()
}

/// Binds and starts the clipboard socket, returning the handle that keeps it alive.
///
/// A bind failure is logged and swallowed on purpose: port forwarding, the
/// credential helper and the CLI shims must keep working even when the socket
/// cannot be served.
pub fn start(handle: Handle) -> Option<ServerHandle> {
    let path = Path::new(cella_protocol::WAYLAND_CLIPBOARD_SOCKET);
    let source = Arc::new(DaemonClipboardSource::new(handle));
    let server = match WaylandClipboardServer::bind(path, source) {
        Ok(server) => server,
        Err(err) => {
            warn!("wayland clipboard socket unavailable: {err}");
            return None;
        }
    };
    match server.spawn() {
        Ok(running) => {
            info!("Wayland clipboard socket serving at {}", path.display());
            Some(running)
        }
        Err(err) => {
            warn!("could not start the wayland clipboard server: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_source_filters_the_targets_sentinel() {
        assert_eq!(
            parse_targets_response(b"TARGETS\ntext/plain\nimage/png\n"),
            vec!["text/plain".to_string(), "image/png".to_string()]
        );
    }

    #[test]
    fn daemon_source_reports_an_empty_clipboard_as_no_targets() {
        assert!(parse_targets_response(b"TARGETS\n").is_empty());
        assert!(parse_targets_response(b"").is_empty());
    }

    #[test]
    fn daemon_source_trims_and_drops_blank_lines() {
        assert_eq!(
            parse_targets_response(b"TARGETS\n  text/plain  \n\n\nimage/png\n"),
            vec!["text/plain".to_string(), "image/png".to_string()]
        );
    }

    #[tokio::test]
    async fn daemon_source_rejects_oversized_publish() {
        let source = DaemonClipboardSource::new(Handle::current());
        let too_big = vec![0u8; MAX_CLIPBOARD_SIZE + 1];
        // The size check runs before any RPC, so this never blocks on a daemon.
        assert!(matches!(
            source.publish("image/png", &too_big),
            Err(SourceError::TooLarge { .. })
        ));
    }
}
