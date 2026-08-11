//! The Wayland event loop: socket binding, client admission, and the host
//! `TARGETS` cache that the dispatch layer reads for every new data device.

use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{Timespec, epoll};
use tracing::{debug, info, warn};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, InitError, ObjectId};
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
pub(crate) struct TargetsCache {
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
pub(crate) struct ServerState {
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
    /// connect. Squatting is prevented by ordering rather than by permissions:
    /// the agent daemon runs from the container entrypoint before `exec "$@"`
    /// starts any user process, so the directory already exists and is
    /// root-owned by the time anything else could try to create it.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Bind`] if the socket is already held by a live
    /// server — `bind_absolute` takes an flock on a sibling lock file, so a
    /// stale socket from a crashed run is cleaned up automatically while a
    /// running one is refused.
    pub fn bind(path: PathBuf, source: Arc<dyn ClipboardSource>) -> Result<Self, ServerError> {
        prepare_socket_dir(&path)?;

        let socket =
            ListeningSocket::bind_absolute(path.clone()).map_err(|source| ServerError::Bind {
                path: path.clone(),
                source,
            })?;
        if let Err(err) = fs::set_permissions(&path, fs::Permissions::from_mode(0o666)) {
            warn!("could not relax permissions on {}: {err}", path.display());
        }

        let mut display = Display::<ServerState>::new().map_err(ServerError::Display)?;
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
            for event in events.drain(..) {
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
            if let Err(err) = self.display.flush_clients() {
                debug!("wayland clipboard flush failed: {err}");
            }
        }
    }

    fn accept_pending(&mut self) {
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

fn prepare_socket_dir(path: &PathBuf) -> Result<(), ServerError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|source| ServerError::Directory {
        path: parent.to_path_buf(),
        source,
    })?;
    if let Err(err) = fs::set_permissions(parent, fs::Permissions::from_mode(0o755)) {
        // Not fatal on its own: if the directory is genuinely unusable, the
        // bind below reports the real reason.
        warn!("could not set mode 0755 on {}: {err}", parent.display());
    }
    Ok(())
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
            WaylandClipboardServer::bind(path.clone(), Arc::new(StubSource::with_targets(&[])))
                .unwrap();
        assert!(path.exists(), "socket should exist after bind");
        drop(server);
        assert!(!path.exists(), "socket should be cleaned up on drop");
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
