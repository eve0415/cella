//! A minimal data-control client, used to drive the server in unit tests.
//!
//! Deliberately raw `wayland-client` rather than `wl-clipboard-rs`: the higher
//! level API only reaches a compositor through `WAYLAND_DISPLAY`, and setting
//! that in-process needs `unsafe` under edition 2024. Connecting a socket
//! directly needs no environment at all. The real `arboard` client is exercised
//! by `tests/arboard_client.rs`, which re-execs to get the env var set.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use crate::{ClipboardSource, ServerHandle, WaylandClipboardServer};

/// Binds a server on a unique path and runs its loop on a thread.
///
/// The tests always need a *running* server: a bound-but-unpolled socket makes
/// every roundtrip hang instead of fail, which is far worse to debug.
pub fn test_server<S: ClipboardSource>(source: S) -> (ServerHandle, PathBuf) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join("cella-wayland-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!(
        "wayland-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let server = WaylandClipboardServer::bind(&path, Arc::new(source)).unwrap();
    (server.spawn().unwrap(), path)
}

#[derive(Default)]
pub struct ClientState {
    pub globals: Vec<(String, u32)>,
    pub seat: Option<WlSeat>,
    pub manager: Option<ExtDataControlManagerV1>,
    pub offers: HashMap<ExtDataControlOfferV1, Vec<String>>,
    /// Set when a `selection` event arrives, whether or not it carries an offer.
    pub saw_selection_event: bool,
    pub selection: Option<ExtDataControlOfferV1>,
    /// Mime the server asked this client to write, during a copy.
    pub asked_for: Option<String>,
    pub cancelled: bool,
    /// What this client writes when the server asks it for a payload.
    pub payload: Vec<u8>,
}

pub struct TestClient {
    pub queue: EventQueue<ClientState>,
    pub state: ClientState,
}

impl TestClient {
    /// Connects and completes the registry roundtrip, binding the seat and the
    /// ext manager if the server advertises them.
    pub fn connect(path: &PathBuf) -> Self {
        let conn = Connection::from_socket(UnixStream::connect(path).unwrap()).unwrap();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = ClientState::default();
        conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut state).unwrap();
        Self { queue, state }
    }

    /// Creates a data device and completes one roundtrip — the single window in
    /// which the server must deliver the offer and the selection.
    pub fn get_device(&mut self) -> ExtDataControlDeviceV1 {
        let qh = self.queue.handle();
        let device = self.state.manager.as_ref().unwrap().get_data_device(
            self.state.seat.as_ref().unwrap(),
            &qh,
            (),
        );
        self.queue.roundtrip(&mut self.state).unwrap();
        device
    }

    /// Mime types on the current selection offer, sorted for stable assertions.
    pub fn selection_mimes(&self) -> Option<Vec<String>> {
        let offer = self.state.selection.as_ref()?;
        let mut mimes = self.state.offers.get(offer).cloned().unwrap_or_default();
        mimes.sort();
        Some(mimes)
    }

    /// Asks the server for one flavor and reads until EOF.
    pub fn receive(&self, mime: &str) -> Vec<u8> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        self.state
            .selection
            .as_ref()
            .unwrap()
            .receive(mime.to_string(), theirs.as_fd());
        drop(theirs);
        self.queue.flush().unwrap();

        let mut buf = Vec::new();
        let mut ours = ours;
        ours.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        ours.read_to_end(&mut buf).unwrap();
        buf
    }

    /// Offers `mimes` to the server and commits the selection, then pumps events
    /// until the server cancels the source — which it does only after it has
    /// read the payload.
    pub fn copy(&mut self, device: &ExtDataControlDeviceV1, mimes: &[&str], payload: Vec<u8>) {
        let qh = self.queue.handle();
        let source = self
            .state
            .manager
            .as_ref()
            .unwrap()
            .create_data_source(&qh, ());
        for mime in mimes {
            source.offer((*mime).to_string());
        }
        self.state.payload = payload;
        device.set_selection(Some(&source));
        // The server blocks reading our payload, so this roundtrip is what
        // delivers the `send` event and lets us answer it.
        self.queue.roundtrip(&mut self.state).unwrap();
    }
}

impl Dispatch<WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        state.globals.push((interface.clone(), version));
        if interface == WlSeat::interface().name && version >= 2 && state.seat.is_none() {
            state.seat = Some(registry.bind(name, 2, qh, ()));
        }
        if interface == ExtDataControlManagerV1::interface().name && state.manager.is_none() {
            state.manager = Some(registry.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<WlSeat, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id, Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.saw_selection_event = true;
                state.selection = id;
            }
            _ => {}
        }
    }

    event_created_child!(Self, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state
                .offers
                .entry(offer.clone())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.asked_for = Some(mime_type);
                let mut file = std::fs::File::from(fd);
                file.write_all(&state.payload).unwrap();
                // Dropping the write end is the EOF the server waits for.
            }
            ext_data_control_source_v1::Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}
