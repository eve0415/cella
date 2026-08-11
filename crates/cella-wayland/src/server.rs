//! The Wayland event loop: socket binding, client admission, and the host
//! `TARGETS` cache that the dispatch layer reads for every new data device.

use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{Timespec, epoll};
use tracing::{debug, info, warn};
use wayland_protocols::ext::data_control::v1::server::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, InitError, ObjectId};
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::{Display, ListeningSocket};

use crate::ClipboardSource;

/// Beyond this many live connections the socket accepts and immediately closes.
///
/// A clipboard endpoint should never need more than a handful; a number this
/// high is a leaking client, and the refusal log is how it becomes visible.
pub const MAX_WAYLAND_CLIENTS: usize = 64;

/// How long a `TARGETS` result stays fresh.
///
/// One Ctrl+V in an `arboard` client opens three connections in quick
/// succession — the primary-selection probe, `file_list()`, and `get_image()` —
/// and each uncached refresh costs a host round trip. Only the format list is
/// cached; payload fetches never are.
const TARGETS_CACHE_TTL: Duration = Duration::from_millis(500);

/// How long the loop blocks in `epoll` before re-checking the shutdown flag.
const POLL_TIMEOUT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 200_000_000,
};

const EPOLL_SOCKET: u64 = 0;
const EPOLL_DISPLAY: u64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to prepare the wayland socket directory {path}: {source}")]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to serve the wayland clipboard socket from {path}: {reason}")]
    UntrustedDirectory { path: PathBuf, reason: String },
    #[error("failed to bind the wayland clipboard socket at {path}: {source:?}")]
    Bind {
        path: PathBuf,
        source: wayland_server::BindError,
    },
    #[error("failed to initialise the wayland display: {0:?}")]
    Display(InitError),
    #[error("failed to set up the wayland event loop: {0}")]
    EventLoop(#[source] std::io::Error),
}

/// The host clipboard's mime types, refreshed at most once per
/// [`TARGETS_CACHE_TTL`].
#[derive(Default)]
pub struct TargetsCache {
    entry: Option<(Instant, Vec<String>)>,
    /// Whether the last refresh failed, so an outage logs once rather than
    /// once per connection.
    down: bool,
}

impl TargetsCache {
    /// Returns the host clipboard's mime types, refreshing at most once per TTL.
    ///
    /// A failed refresh yields an empty list rather than an error: the caller's
    /// only recourse is to omit the selection event, and that is the documented
    /// degradation path.
    pub(crate) fn get(&mut self, source: &dyn ClipboardSource) -> Vec<String> {
        if let Some((at, mimes)) = &self.entry
            && at.elapsed() < TARGETS_CACHE_TTL
        {
            return mimes.clone();
        }
        let mimes = match source.targets() {
            Ok(mimes) => {
                if self.down {
                    info!("clipboard bridge recovered");
                    self.down = false;
                }
                mimes
            }
            Err(err) => {
                if !self.down {
                    warn!("clipboard bridge unavailable, serving empty clipboard: {err}");
                    self.down = true;
                }
                Vec::new()
            }
        };
        self.entry = Some((Instant::now(), mimes.clone()));
        mimes
    }
}

/// Everything the dispatch impls need. One instance per server, shared across
/// all connected clients.
pub struct ServerState {
    pub(crate) source: Arc<dyn ClipboardSource>,
    pub(crate) targets: TargetsCache,
    /// Mime types each live data source has offered, keyed by the source
    /// resource. Populated by `offer` requests, read when the client commits
    /// the selection.
    pub(crate) pending_sources: HashMap<ObjectId, Vec<String>>,
}

/// Decrements the live-client count when a connection goes away.
#[derive(Debug)]
struct ClientCounter(Arc<AtomicUsize>);

impl ClientData for ClientCounter {
    fn initialized(&self, _client_id: ClientId) {}

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct WaylandClipboardServer {
    socket: ListeningSocket,
    display: Display<ServerState>,
    state: ServerState,
    epoll: OwnedFd,
    clients: Arc<AtomicUsize>,
}

impl WaylandClipboardServer {
    /// Binds the clipboard socket at an absolute path.
    ///
    /// The parent directory is created `0755` and the socket itself is left
    /// world-writable (`0666`) so the container's non-root remote user can
    /// connect. Squatting is prevented by the directory, not by the socket
    /// mode: once `/tmp/cella` is root-owned and not group/other-writable, no
    /// other container process can unlink or replace what is inside it. That
    /// property is checked rather than assumed — see [`validate_socket_dir`].
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::UntrustedDirectory`] if the parent directory is a
    /// symlink, is owned by another user, or is writable by anyone else, and
    /// [`ServerError::Bind`] if the socket is already held by a live server —
    /// `bind_absolute` takes an flock on a sibling lock file, so a stale socket
    /// from a crashed run is cleaned up automatically while a running one is
    /// refused.
    pub fn bind(path: &Path, source: Arc<dyn ClipboardSource>) -> Result<Self, ServerError> {
        prepare_socket_dir(path)?;

        let socket = ListeningSocket::bind_absolute(path.to_path_buf()).map_err(|source| {
            ServerError::Bind {
                path: path.to_path_buf(),
                source,
            }
        })?;
        if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o666)) {
            warn!("could not relax permissions on {}: {err}", path.display());
        }

        let mut display = Display::<ServerState>::new().map_err(ServerError::Display)?;
        create_globals(&display);
        let epoll = build_epoll(&socket, &mut display)?;

        Ok(Self {
            socket,
            display,
            state: ServerState {
                source,
                targets: TargetsCache::default(),
                pending_sources: HashMap::new(),
            },
            epoll,
            clients: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Runs the event loop on a dedicated thread.
    ///
    /// Deliberately a `std::thread` and not a tokio task: the loop blocks, and
    /// [`ClipboardSource`] implementations block on a runtime handle, which
    /// panics if called from a runtime worker.
    ///
    /// # Errors
    ///
    /// Returns the OS error if the thread cannot be spawned.
    pub fn spawn(self) -> std::io::Result<ServerHandle> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let join = std::thread::Builder::new()
            .name("cella-wayland".to_string())
            .spawn(move || self.run(&flag))?;
        Ok(ServerHandle {
            shutdown,
            join: Some(join),
        })
    }

    fn run(mut self, shutdown: &AtomicBool) {
        let mut events = Vec::with_capacity(4);
        while !shutdown.load(Ordering::Relaxed) {
            if let Err(err) = epoll::wait(
                &self.epoll,
                rustix::buffer::spare_capacity(&mut events),
                Some(&POLL_TIMEOUT),
            ) {
                warn!("wayland clipboard event loop poll failed: {err}");
                return;
            }
            for event in &events {
                match event.data.u64() {
                    EPOLL_SOCKET => self.accept_pending(),
                    EPOLL_DISPLAY => {
                        if let Err(err) = self.display.dispatch_clients(&mut self.state) {
                            warn!("wayland clipboard dispatch failed: {err}");
                        }
                    }
                    other => debug!("unexpected wayland epoll event {other}"),
                }
            }
            events.clear();
            if let Err(err) = self.display.flush_clients() {
                debug!("wayland clipboard flush failed: {err}");
            }
        }
    }

    fn accept_pending(&self) {
        loop {
            match self.socket.accept() {
                Ok(Some(stream)) => {
                    let live = self.clients.load(Ordering::SeqCst);
                    if live >= MAX_WAYLAND_CLIENTS {
                        warn!("wayland clipboard socket at {live} clients; refusing connection");
                        drop(stream);
                        continue;
                    }
                    self.clients.fetch_add(1, Ordering::SeqCst);
                    if let Err(err) = self
                        .display
                        .handle()
                        .insert_client(stream, Arc::new(ClientCounter(Arc::clone(&self.clients))))
                    {
                        self.clients.fetch_sub(1, Ordering::SeqCst);
                        warn!("failed to admit wayland clipboard client: {err}");
                    }
                }
                Ok(None) => return,
                Err(err) => {
                    warn!("wayland clipboard accept failed: {err}");
                    return;
                }
            }
        }
    }
}

/// Stops the server thread and removes the socket when dropped.
pub struct ServerHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn prepare_socket_dir(path: &Path) -> Result<(), ServerError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    // A no-op when the path already exists — including when it exists as a
    // symlink, which is what `validate_socket_dir` is here to catch.
    fs::create_dir_all(parent).map_err(|source| ServerError::Directory {
        path: parent.to_path_buf(),
        source,
    })?;
    // Validate *before* chmod, never after: `chmod(2)` follows symlinks, so
    // relaxing the mode first would hand an attacker-planted link a
    // root-privileged permission change on a directory of their choosing.
    validate_socket_dir(parent)?;
    if let Err(err) = fs::set_permissions(parent, fs::Permissions::from_mode(0o755)) {
        // Not fatal on its own: if the directory is genuinely unusable, the
        // bind below reports the real reason.
        warn!("could not set mode 0755 on {}: {err}", parent.display());
    }
    Ok(())
}

/// Refuses to serve out of a directory another user could have prepared.
///
/// The socket lives under `/tmp`, which cella deliberately leaves mode 1777, and
/// the agent is restarted as root long after container processes are running —
/// every `cella up` calls `restart_agent_in_container`. So "the daemon starts
/// before any user process" holds only for a container's very first start, and
/// cannot be the sole defence. Three properties make the path trustworthy:
/// it is a real directory rather than a symlink, it is owned by root or by us,
/// and no one else can write to it.
fn validate_socket_dir(dir: &Path) -> Result<(), ServerError> {
    let untrusted = |reason: &str| ServerError::UntrustedDirectory {
        path: dir.to_path_buf(),
        reason: reason.to_string(),
    };
    // `symlink_metadata` deliberately does not follow the final component.
    let meta = fs::symlink_metadata(dir).map_err(|source| ServerError::Directory {
        path: dir.to_path_buf(),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(untrusted("it is a symlink"));
    }
    if !meta.is_dir() {
        return Err(untrusted("it is not a directory"));
    }
    let euid = rustix::process::geteuid().as_raw();
    if meta.uid() != 0 && meta.uid() != euid {
        return Err(untrusted(&format!(
            "it is owned by uid {} rather than root or uid {euid}",
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(untrusted(&format!(
            "it is group- or world-writable (mode {:04o})",
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

/// Advertises the clipboard endpoint and nothing else.
///
/// No `wl_compositor`, `wl_shm`, or outputs — this is not a compositor. The wlr
/// manager is pinned to version 1 on purpose: version 2 promises a primary
/// selection channel, and clients that bind it then expect a
/// `primary_selection` event cella never sends.
fn create_globals(display: &Display<ServerState>) {
    let dh = display.handle();
    dh.create_global::<ServerState, WlSeat, _>(2, ());
    dh.create_global::<ServerState, ExtDataControlManagerV1, _>(1, ());
    dh.create_global::<ServerState, ZwlrDataControlManagerV1, _>(1, ());
}

fn build_epoll(
    socket: &ListeningSocket,
    display: &mut Display<ServerState>,
) -> Result<OwnedFd, ServerError> {
    let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(io_err)?;
    epoll::add(
        &epoll,
        socket,
        epoll::EventData::new_u64(EPOLL_SOCKET),
        epoll::EventFlags::IN,
    )
    .map_err(io_err)?;
    epoll::add(
        &epoll,
        display.backend().poll_fd(),
        epoll::EventData::new_u64(EPOLL_DISPLAY),
        epoll::EventFlags::IN,
    )
    .map_err(io_err)?;
    Ok(epoll)
}

fn io_err(err: rustix::io::Errno) -> ServerError {
    ServerError::EventLoop(std::io::Error::from(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stubs::{CountingSource, StubSource};
    use std::sync::Arc;

    #[test]
    fn binds_an_absolute_socket_path_and_removes_it_on_drop() {
        let dir = std::env::temp_dir().join("cella-wayland-test-bind");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wayland-test-0");
        let _ = fs::remove_file(&path);

        let server =
            WaylandClipboardServer::bind(&path, Arc::new(StubSource::with_targets(&[]))).unwrap();
        assert!(path.exists(), "socket should exist after bind");
        drop(server);
        assert!(!path.exists(), "socket should be cleaned up on drop");
    }

    /// Regression: `/tmp` is mode 1777 in cella containers, and the agent is
    /// restarted as root long after user processes exist (every `cella up`
    /// calls `restart_agent_in_container`). A container process could plant
    /// `/tmp/cella` as a symlink; `chmod(2)` follows it, so the old code
    /// relaxed the mode of an attacker-chosen directory as root.
    #[test]
    fn refuses_a_socket_dir_that_is_a_symlink() {
        let root = std::env::temp_dir().join("cella-wayland-test-symlink");
        let _ = fs::remove_dir_all(&root);
        let victim = root.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o700)).unwrap();
        let planted = root.join("planted");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        let err = WaylandClipboardServer::bind(
            &planted.join("wayland-0"),
            Arc::new(StubSource::with_targets(&[])),
        )
        .err()
        .expect("must refuse to bind through a symlinked socket dir");
        assert!(
            matches!(err, ServerError::UntrustedDirectory { .. }),
            "got {err:?}"
        );
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o700,
            "the symlink target's mode must be untouched"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A group- or world-writable socket directory lets any container process
    /// unlink the socket and bind its own in its place.
    #[test]
    fn refuses_a_world_writable_socket_dir() {
        let dir = std::env::temp_dir().join("cella-wayland-test-loose-dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        let err = WaylandClipboardServer::bind(
            &dir.join("wayland-0"),
            Arc::new(StubSource::with_targets(&[])),
        )
        .err()
        .expect("must refuse a world-writable socket dir");
        assert!(
            matches!(err, ServerError::UntrustedDirectory { .. }),
            "got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn accepts_a_socket_dir_it_owns_with_tight_permissions() {
        let dir = std::env::temp_dir().join("cella-wayland-test-good-dir");
        let _ = fs::remove_dir_all(&dir);
        let server = WaylandClipboardServer::bind(
            &dir.join("wayland-0"),
            Arc::new(StubSource::with_targets(&[])),
        )
        .expect("a freshly created dir must be accepted");
        drop(server);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn targets_cache_collapses_repeated_calls_within_ttl() {
        let source = Arc::new(CountingSource::default());
        let mut cache = TargetsCache::default();
        let _ = cache.get(source.as_ref());
        let _ = cache.get(source.as_ref());
        assert_eq!(
            source.calls(),
            1,
            "second call within TTL must hit the cache"
        );
    }
}
