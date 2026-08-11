//! The data-control protocol surface.
//!
//! Two protocol families carry the same six requests: the staging
//! `ext_data_control_v1` and the deprecated `zwlr_data_control_v1`. Their
//! generated types are distinct, so the dispatch impls are produced by
//! [`data_control_family!`] from one body — the two paths cannot drift.

use std::io::{ErrorKind, Read as _, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};
use wayland_protocols::ext::data_control::v1::server::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_server::backend::ClientId;
use wayland_server::protocol::wl_seat::{self, WlSeat};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use cella_protocol::MAX_CLIPBOARD_SIZE;

use crate::ClipboardSource;
use crate::server::ServerState;

/// How long a copy waits for the owning client to write its payload.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Preference order used when a client offers several flavors at once.
///
/// `ClipboardCopy` carries exactly one mime and the macOS backend replaces the
/// whole pasteboard per call, so one flavor has to win. Kept separate so a
/// future multi-flavor copy is an additive change rather than a rewrite.
fn pick_mime(mimes: &[String]) -> Option<String> {
    const PREFERRED: [&str; 4] = [
        "image/png",
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
    ];
    PREFERRED
        .iter()
        .find(|want| mimes.iter().any(|m| m == *want))
        .map(|want| (*want).to_string())
        .or_else(|| mimes.iter().find(|m| is_text(m)).cloned())
        .or_else(|| mimes.first().cloned())
}

/// Mirrors `wl_clipboard_rs::utils::is_text`, which is what clients use to
/// decide whether an offer satisfies a plain-text paste.
fn is_text(mime_type: &str) -> bool {
    match mime_type {
        "TEXT" | "STRING" | "UTF8_STRING" => true,
        x if x.starts_with("text/") => true,
        x if x.contains("json")
            || x.ends_with("script")
            || x.ends_with("xml")
            || x.ends_with("yaml")
            || x.ends_with("csv")
            || x.ends_with("ini") =>
        {
            true
        }
        _ => false,
    }
}

/// Serves one `receive` request on a worker thread.
///
/// A payload fetch costs a full daemon round trip, and the server thread has to
/// stay free for the other connections a single paste opens. Dropping the fd at
/// the end of the closure is the EOF the client is waiting for.
fn spawn_receive(source: Arc<dyn ClipboardSource>, mime_type: String, fd: OwnedFd) {
    let requested = mime_type.clone();
    let spawned = std::thread::Builder::new()
        .name("cella-wayland-send".to_string())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            match source.fetch(&mime_type) {
                Ok(bytes) if bytes.len() > MAX_CLIPBOARD_SIZE => {
                    // Never truncate: a short PNG is worse than no PNG.
                    warn!(
                        "clipboard payload for {mime_type} is {} bytes, over the {MAX_CLIPBOARD_SIZE} byte cap; serving nothing",
                        bytes.len()
                    );
                }
                Ok(bytes) => {
                    if let Err(err) = file.write_all(&bytes) {
                        debug!("client closed the {mime_type} pipe early: {err}");
                    }
                }
                Err(err) => warn!("clipboard fetch for {mime_type} failed: {err}"),
            }
        });
    if let Err(err) = spawned {
        warn!("could not spawn clipboard sender for {requested}: {err}");
    }
}

/// Reads at most one payload from a client, refusing anything over the cap.
fn read_capped(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    stream
        .set_read_timeout(Some(CLIENT_READ_TIMEOUT))
        .map_err(|e| format!("could not arm the read timeout: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_CLIPBOARD_SIZE {
                    return Err(format!(
                        "payload exceeds the {MAX_CLIPBOARD_SIZE} byte cap; refusing to publish"
                    ));
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                return Err(format!(
                    "client did not finish writing within {CLIENT_READ_TIMEOUT:?}"
                ));
            }
            Err(err) => return Err(format!("read from the owning client failed: {err}")),
        }
    }
}

/// Pulls one flavor out of a client-owned selection and publishes it to the host.
///
/// `send` hands the write end to the client. The flush is what actually puts the
/// fd on the wire, so it has to happen before we let go of our copy and before
/// we start reading, or the client never learns it is meant to write.
fn transfer_selection<F>(state: &ServerState, dh: &DisplayHandle, mime: &str, send: F)
where
    F: FnOnce(BorrowedFd<'_>),
{
    let (mut ours, theirs) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(err) => {
            warn!("could not create a clipboard transfer pipe: {err}");
            return;
        }
    };
    send(theirs.as_fd());
    if let Err(err) = dh.clone().flush_clients() {
        warn!("could not hand the clipboard pipe to the client: {err}");
        return;
    }
    drop(theirs);

    match read_capped(&mut ours) {
        Ok(bytes) => {
            if let Err(err) = state.source.publish(mime, &bytes) {
                warn!("could not publish {mime} to the host clipboard: {err}");
            }
        }
        Err(err) => warn!("clipboard copy of {mime} abandoned: {err}"),
    }
}

impl GlobalDispatch<WlSeat, ()> for ServerState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        // data-control needs no input capabilities; advertise none honestly.
        // Both events matter: clients filter seats on version >= 2 and match
        // them by name, so a nameless seat is an invisible seat.
        seat.capabilities(wl_seat::Capability::empty());
        seat.name("cella".to_string());
    }
}

impl Dispatch<WlSeat, ()> for ServerState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _seat: &WlSeat,
        request: wl_seat::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // We advertise no capabilities, so a conforming client never asks for
        // an input device. Answering with the protocol error beats leaving the
        // object uninitialized, which panics the dispatch loop.
        let missing = wl_seat::Error::MissingCapability;
        match request {
            wl_seat::Request::GetPointer { id } => {
                data_init.post_error(id, missing, "cella's seat has no pointer");
            }
            wl_seat::Request::GetKeyboard { id } => {
                data_init.post_error(id, missing, "cella's seat has no keyboard");
            }
            wl_seat::Request::GetTouch { id } => {
                data_init.post_error(id, missing, "cella's seat has no touch");
            }
            _ => {}
        }
    }
}

/// Generates the manager/device/offer/source dispatch for one protocol family.
macro_rules! data_control_family {
    (
        manager: $manager:ty, $manager_req:path;
        device:  $device:ty,  $device_req:path;
        offer:   $offer:ty,   $offer_req:path;
        source:  $source:ty,  $source_req:path;
    ) => {
        impl GlobalDispatch<$manager, ()> for ServerState {
            fn bind(
                _state: &mut Self,
                _handle: &DisplayHandle,
                _client: &Client,
                resource: New<$manager>,
                _global_data: &(),
                data_init: &mut DataInit<'_, Self>,
            ) {
                data_init.init(resource, ());
            }
        }

        impl Dispatch<$manager, ()> for ServerState {
            fn request(
                state: &mut Self,
                client: &Client,
                manager: &$manager,
                request: <$manager as Resource>::Request,
                _data: &(),
                dh: &DisplayHandle,
                data_init: &mut DataInit<'_, Self>,
            ) {
                use $manager_req as Req;
                match request {
                    Req::GetDataDevice { id, seat: _ } => {
                        let device = data_init.init(id, ());
                        // Blocking here is deliberate: the offer and the
                        // selection have to land inside the client's first
                        // roundtrip, and there is no later chance.
                        let mimes = state.targets.get(state.source.as_ref());
                        if mimes.is_empty() {
                            // Deliberately send nothing. The client reads the
                            // absence as an empty clipboard, which is honest;
                            // an offer with zero mime types reads as broken.
                            return;
                        }
                        let Ok(offer) =
                            client.create_resource::<$offer, (), Self>(dh, manager.version(), ())
                        else {
                            warn!("could not create a data offer for a clipboard client");
                            return;
                        };
                        device.data_offer(&offer);
                        for mime in mimes {
                            offer.offer(mime);
                        }
                        device.selection(Some(&offer));
                    }
                    Req::CreateDataSource { id } => {
                        let source = data_init.init(id, ());
                        state.pending_sources.insert(source.id(), Vec::new());
                    }
                    _ => {}
                }
            }
        }

        impl Dispatch<$device, ()> for ServerState {
            fn request(
                state: &mut Self,
                _client: &Client,
                _device: &$device,
                request: <$device as Resource>::Request,
                _data: &(),
                dh: &DisplayHandle,
                _data_init: &mut DataInit<'_, Self>,
            ) {
                use $device_req as Req;
                let Req::SetSelection { source: Some(src) } = request else {
                    // A cleared selection is not propagated: cella never wipes
                    // the host clipboard on a client's behalf.
                    debug!("ignoring a data-control selection with no source");
                    return;
                };
                let mimes = state
                    .pending_sources
                    .get(&src.id())
                    .cloned()
                    .unwrap_or_default();
                let Some(chosen) = pick_mime(&mimes) else {
                    src.cancelled();
                    return;
                };
                transfer_selection(state, dh, &chosen, |fd| src.send(chosen.clone(), fd));
                // Only after the read: cancelling first lets a client such as
                // `wl-copy --foreground` exit before it writes anything.
                src.cancelled();
            }
        }

        impl Dispatch<$offer, ()> for ServerState {
            fn request(
                state: &mut Self,
                _client: &Client,
                _offer: &$offer,
                request: <$offer as Resource>::Request,
                _data: &(),
                _dh: &DisplayHandle,
                _data_init: &mut DataInit<'_, Self>,
            ) {
                use $offer_req as Req;
                if let Req::Receive { mime_type, fd } = request {
                    spawn_receive(Arc::clone(&state.source), mime_type, fd);
                }
            }
        }

        impl Dispatch<$source, ()> for ServerState {
            fn request(
                state: &mut Self,
                _client: &Client,
                source: &$source,
                request: <$source as Resource>::Request,
                _data: &(),
                _dh: &DisplayHandle,
                _data_init: &mut DataInit<'_, Self>,
            ) {
                use $source_req as Req;
                if let Req::Offer { mime_type } = request {
                    state
                        .pending_sources
                        .entry(source.id())
                        .or_default()
                        .push(mime_type);
                }
            }

            fn destroyed(state: &mut Self, _client: ClientId, resource: &$source, _data: &()) {
                state.pending_sources.remove(&resource.id());
            }
        }
    };
}

data_control_family! {
    manager: ExtDataControlManagerV1, ext_data_control_manager_v1::Request;
    device:  ExtDataControlDeviceV1,  wayland_protocols::ext::data_control::v1::server::ext_data_control_device_v1::Request;
    offer:   ExtDataControlOfferV1,   ext_data_control_offer_v1::Request;
    source:  ExtDataControlSourceV1,  ext_data_control_source_v1::Request;
}

data_control_family! {
    manager: ZwlrDataControlManagerV1, zwlr_data_control_manager_v1::Request;
    device:  ZwlrDataControlDeviceV1,  wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_device_v1::Request;
    offer:   ZwlrDataControlOfferV1,   zwlr_data_control_offer_v1::Request;
    source:  ZwlrDataControlSourceV1,  zwlr_data_control_source_v1::Request;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stubs::StubSource;
    use crate::testclient::{TestClient, test_server};

    #[test]
    fn advertises_seat_and_both_managers() {
        let (_server, path) = test_server(Arc::new(StubSource::with_targets(&["text/plain"])));
        let globals = TestClient::connect(&path).state.globals;
        assert!(
            globals.iter().any(|(i, v)| i == "wl_seat" && *v >= 2),
            "got {globals:?}"
        );
        assert!(
            globals
                .iter()
                .any(|(i, v)| i == "ext_data_control_manager_v1" && *v == 1),
            "got {globals:?}"
        );
        assert!(
            globals
                .iter()
                .any(|(i, v)| i == "zwlr_data_control_manager_v1" && *v == 1),
            "got {globals:?}"
        );
    }

    #[test]
    fn get_data_device_emits_offer_with_host_mimes_in_first_roundtrip() {
        let (_server, path) = test_server(Arc::new(StubSource::with_targets(&[
            "image/png",
            "text/plain",
        ])));
        let mut client = TestClient::connect(&path);
        let _device = client.get_device();
        assert_eq!(
            client.selection_mimes(),
            Some(vec!["image/png".to_string(), "text/plain".to_string()]),
            "the offer must arrive inside the first roundtrip"
        );
    }

    /// Regression for the cached-format bug: a client that connected while the
    /// host held text must not make the *next* client see text after the host
    /// switched to an image. This is the exact sequence a user performs when
    /// they copy a screenshot and immediately paste it in the container.
    #[test]
    fn a_new_device_sees_the_host_clipboard_as_it_is_now() {
        let source = Arc::new(StubSource::with_targets(&["text/plain"]));
        let (_server, path) = test_server(Arc::clone(&source) as Arc<dyn ClipboardSource>);

        let mut first = TestClient::connect(&path);
        let _ = first.get_device();
        assert_eq!(
            first.selection_mimes(),
            Some(vec!["text/plain".to_string()])
        );

        source.replace_contents(vec![("image/png".to_string(), b"\x89PNG".to_vec())]);

        let mut second = TestClient::connect(&path);
        let _ = second.get_device();
        assert_eq!(
            second.selection_mimes(),
            Some(vec!["image/png".to_string()]),
            "a device opened right after the host clipboard changed must see the new format"
        );
        assert_eq!(second.receive("image/png"), b"\x89PNG");
    }

    #[test]
    fn receive_writes_payload_then_closes_fd() {
        let (_server, path) = test_server(Arc::new(StubSource::new(vec![(
            "image/png".to_string(),
            b"\x89PNG-bytes".to_vec(),
        )])));
        let mut client = TestClient::connect(&path);
        let _device = client.get_device();
        assert_eq!(client.receive("image/png"), b"\x89PNG-bytes");
    }

    #[test]
    fn no_selection_event_when_source_is_unavailable() {
        let (_server, path) = test_server(Arc::new(StubSource::unavailable()));
        let mut client = TestClient::connect(&path);
        let _device = client.get_device();
        assert!(
            !client.state.saw_selection_event,
            "a down bridge must look like an empty clipboard, not a broken offer"
        );
        assert!(client.state.selection.is_none());
    }

    #[test]
    fn empty_host_clipboard_emits_no_selection_event() {
        let (_server, path) = test_server(Arc::new(StubSource::with_targets(&[])));
        let mut client = TestClient::connect(&path);
        let _device = client.get_device();
        assert!(!client.state.saw_selection_event);
    }

    #[test]
    fn set_selection_publishes_preferred_mime_then_cancels_source() {
        let source = Arc::new(StubSource::with_targets(&["text/plain"]));
        let (_server, path) = test_server(Arc::clone(&source) as Arc<dyn ClipboardSource>);
        let mut client = TestClient::connect(&path);
        let device = client.get_device();

        client.copy(&device, &["text/plain", "image/png"], b"cella".to_vec());

        assert_eq!(
            client.state.asked_for.as_deref(),
            Some("image/png"),
            "png must win the preference order"
        );
        assert_eq!(
            source.published(),
            vec![("image/png".to_string(), b"cella".to_vec())]
        );
        assert!(
            client.state.cancelled,
            "the source is cancelled once the host owns the selection"
        );
    }

    #[test]
    fn payload_over_cap_serves_nothing() {
        let oversized = vec![0u8; MAX_CLIPBOARD_SIZE + 1];
        let (_server, path) = test_server(Arc::new(StubSource::new(vec![(
            "image/png".to_string(),
            oversized,
        )])));
        let mut client = TestClient::connect(&path);
        let _device = client.get_device();
        assert!(
            client.receive("image/png").is_empty(),
            "never truncate — a short PNG is worse than no PNG"
        );
    }

    #[test]
    fn copy_over_cap_is_not_published() {
        let source = Arc::new(StubSource::with_targets(&["text/plain"]));
        let (_server, path) = test_server(Arc::clone(&source) as Arc<dyn ClipboardSource>);
        let mut client = TestClient::connect(&path);
        let device = client.get_device();

        client.copy(&device, &["text/plain"], vec![b'x'; MAX_CLIPBOARD_SIZE + 1]);

        assert!(
            source.published().is_empty(),
            "the host clipboard must be left alone when the payload is over the cap"
        );
    }

    #[test]
    fn picks_png_over_text() {
        let mimes = ["text/plain".to_string(), "image/png".to_string()];
        assert_eq!(pick_mime(&mimes).as_deref(), Some("image/png"));
    }

    #[test]
    fn picks_utf8_text_over_bare_text_plain() {
        let mimes = [
            "text/plain".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            pick_mime(&mimes).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn falls_back_to_any_text_then_to_anything() {
        assert_eq!(
            pick_mime(&[
                "application/octet-stream".to_string(),
                "text/html".to_string()
            ])
            .as_deref(),
            Some("text/html")
        );
        assert_eq!(
            pick_mime(&["application/octet-stream".to_string()]).as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(pick_mime(&[]), None);
    }
}
