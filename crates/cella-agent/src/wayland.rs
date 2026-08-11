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
use tracing::{debug, info, warn};

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

/// Whether this container was created with cella's clipboard socket enabled.
///
/// `settings.clipboard.wayland` lives on the host, and the agent never sees the
/// config — `WAYLAND_DISPLAY` is the only channel. Requiring it to match the
/// socket path exactly means turning the setting off actually stops the socket
/// being served, rather than only hiding it, and it leaves a container that
/// points at some other compositor alone.
fn should_serve(display_env: Option<&str>) -> bool {
    display_env == Some(cella_protocol::WAYLAND_CLIPBOARD_SOCKET)
}

/// Binds and starts the clipboard socket, returning the handle that keeps it alive.
///
/// A bind failure is logged and swallowed on purpose: port forwarding, the
/// credential helper and the CLI shims must keep working even when the socket
/// cannot be served.
pub fn start(handle: Handle) -> Option<ServerHandle> {
    let display_env = std::env::var("WAYLAND_DISPLAY").ok();
    if !should_serve(display_env.as_deref()) {
        debug!("clipboard.wayland is off for this container; not serving the socket");
        return None;
    }
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

    /// Regression: the socket used to be served unconditionally, so turning
    /// `clipboard.wayland` off only stopped `WAYLAND_DISPLAY` being injected —
    /// the endpoint stayed live and reachable regardless of the setting.
    #[test]
    fn does_not_serve_when_the_container_was_created_with_the_setting_off() {
        assert!(!should_serve(None));
    }

    #[test]
    fn does_not_hijack_a_container_pointed_at_another_compositor() {
        assert!(!should_serve(Some("wayland-1")));
        assert!(!should_serve(Some("/run/user/1000/wayland-0")));
    }

    #[test]
    fn serves_when_the_container_was_created_with_the_setting_on() {
        assert!(should_serve(Some(cella_protocol::WAYLAND_CLIPBOARD_SOCKET)));
    }

    /// Names the child half of the live round trip below.
    const LIVE_CHILD: &str = "CELLA_WAYLAND_LIVE_CHILD";

    /// Drives the real `arboard` against a socket backed by the real daemon.
    ///
    /// Everything else in this crate stubs the host away. This is the only test
    /// that exercises the whole chain — arboard, the Wayland socket,
    /// `DaemonClipboardSource`, the control channel, and the host pasteboard —
    /// so it is also the only one that can catch a break between them. Skips
    /// when no daemon is reachable, which is the normal case in CI.
    #[tokio::test(flavor = "multi_thread")]
    async fn live_arboard_round_trip_against_the_daemon() {
        if std::env::var(LIVE_CHILD).is_ok() {
            return;
        }
        if crate::control::resolve_daemon_connection().is_err() {
            eprintln!("skipping: no cella daemon reachable from this container");
            return;
        }
        // Probe with the async API rather than `source.targets()`: this test
        // body runs on a tokio worker, and `ClipboardSource` blocks. In
        // production every caller is a plain `std::thread`, which is the whole
        // reason `spawn()` does not use a tokio task.
        let Ok(raw) = request_clipboard_paste(TARGETS_SENTINEL).await else {
            eprintln!("skipping: daemon reachable but the clipboard bridge did not answer");
            return;
        };
        let targets = parse_targets_response(&raw);
        let source = DaemonClipboardSource::new(Handle::current());

        let dir = std::env::temp_dir().join("cella-wayland-live");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("wayland-{}", std::process::id()));
        let server = WaylandClipboardServer::bind(&path, Arc::new(source)).unwrap();
        let _running = server.spawn().unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wayland::tests::live_arboard_child",
                "--nocapture",
            ])
            .env(LIVE_CHILD, targets.join(","))
            .env("WAYLAND_DISPLAY", &path)
            .env_remove("DISPLAY")
            .status()
            .unwrap();
        assert!(status.success(), "arboard failed against the live socket");
    }

    /// The child half: runs inside the re-execed process with `WAYLAND_DISPLAY`
    /// set. Kept as its own test so the parent can name it with `--exact`.
    #[test]
    fn live_arboard_child() {
        let Ok(targets) = std::env::var(LIVE_CHILD) else {
            return;
        };
        let mut clipboard =
            arboard::Clipboard::new().expect("Clipboard::new() against the live cella socket");
        if targets.split(',').any(|t| t == "image/png") {
            let image = clipboard.get_image().expect("get_image() from the host");
            assert!(image.width > 0 && image.height > 0);
            eprintln!("live image: {}x{}", image.width, image.height);
        } else if targets.split(',').any(|t| t.starts_with("text/")) {
            eprintln!("live text: {} bytes", clipboard.get_text().unwrap().len());
        }
    }
}
