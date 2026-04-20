//! Per-workspace SSH-agent TCP bridge.
//!
//! Bridges the host's `$SSH_AUTH_SOCK` over a TCP listener that the
//! in-container `cella-agent` connects to. The agent then exposes a
//! Unix socket inside the container — created in the container's own
//! filesystem, not bind-mounted from the host — so consumers like
//! `ssh-add` and `git commit -S` see a normal `SSH_AUTH_SOCK` path.
//!
//! ## Why TCP instead of a host-side Unix socket
//!
//! The earlier design (a Unix socket on the macOS host bind-mounted
//! into the container) failed empirically on colima:
//!
//! ```text
//! docker: Error response from daemon: error while creating mount source
//!   path '/Users/u/.cella/run/cella-virtiofs-probe.sock':
//!   mkdir <path>: operation not supported
//! ```
//!
//! Colima virtiofs rejects `mkdir` for any host-side Unix-socket path,
//! and Docker's mkdir-source-if-missing fallback fires on every bind-mount
//! attempt. There's no way to surface a host-created Unix socket as a
//! working bind mount inside a colima container.
//!
//! TCP sidesteps this entirely: cella-daemon already exposes a localhost
//! TCP control port that containers reach via `host.docker.internal`
//! (or the equivalent). A second TCP listener for ssh-agent traffic
//! follows the same path, and bytes flow without ever touching the
//! virtiofs mount layer. This mirrors VS Code's working approach
//! (vscode-server inside the container creates `/tmp/vscode-ssh-auth-
//! <uuid>.sock` and bridges back to the host through the existing
//! Remote-SSH channel) and cella's own TCP credential proxy from
//! commit `9360b50`.
//!
//! ## Architecture
//!
//! ```text
//! container                          daemon (macOS host)
//! ─────────                          ───────────────────
//! ssh-add ─→ /run/host-services/     ┌─────────────────────────────┐
//!            ssh-auth.sock           │ TCP listener (port N)       │
//!              ↑ created by          │   accept ──→ UnixStream::    │
//!              cella-agent           │     connect($SSH_AUTH_SOCK) │
//!              ↓ each conn           │   copy_bidirectional         │
//!            TCP to host:N ────→─────┘                              │
//!                                                                   ↓
//!                                                       1Password / ssh-agent
//! ```
//!
//! Lifecycle: refcounted per workspace folder. First `cella up` for a
//! workspace allocates the listener; subsequent ups for the same workspace
//! reuse it (refcount++). The listener is torn down when the refcount
//! reaches zero.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{Mutex, watch};
use tokio::task::AbortHandle;
use tracing::{debug, warn};

use crate::CellaDaemonError;

/// Shared bridge manager state.
pub type SharedSshProxyManager = Arc<Mutex<SshProxyManager>>;

/// Refcount-keyed registry of per-workspace SSH-agent TCP bridges.
pub struct SshProxyManager {
    state_dir: PathBuf,
    daemon_pid: u32,
    auth_token: String,
    bridges: HashMap<PathBuf, BridgeEntry>,
}

struct BridgeEntry {
    upstream_socket: PathBuf,
    bridge_port: u16,
    refcount: usize,
    accept_task: AbortHandle,
    /// Broadcast channel that signals teardown to the accept loop and to
    /// every spawned per-connection bridge. Sending `true` causes the
    /// accept loop and any in-flight bridges to drop their streams,
    /// which closes the corresponding fds and EOFs both peers.
    shutdown_tx: watch::Sender<bool>,
}

impl SshProxyManager {
    /// Create a new manager that writes its state file under `state_dir`.
    /// The `daemon_pid` and `auth_token` are recorded in the persisted
    /// state file so external readers (e.g. a future `cella doctor` probe)
    /// can verify liveness and reconstruct the bridge auth handshake.
    #[must_use]
    pub fn new(state_dir: PathBuf, auth_token: String) -> Self {
        Self::with_pid(state_dir, auth_token, std::process::id())
    }

    /// Construct a manager with an explicit `daemon_pid`. Tests use this
    /// so state-file assertions are stable across runs.
    #[must_use]
    pub fn with_pid(state_dir: PathBuf, auth_token: String, daemon_pid: u32) -> Self {
        Self {
            state_dir,
            daemon_pid,
            auth_token,
            bridges: HashMap::new(),
        }
    }

    /// Register a bridge for `workspace` connecting to `upstream_socket`.
    /// Returns the TCP port the in-container agent should connect to.
    ///
    /// On first registration this binds a `TcpListener` on `127.0.0.1:0`
    /// and spawns an accept-loop that, for each connection accepted from
    /// the in-container agent, opens a fresh `UnixStream::connect(upstream
    /// _socket)` and bidirectionally copies bytes between the two.
    ///
    /// Subsequent register behavior depends on whether the `upstream_socket`
    /// matches the existing entry:
    /// - **Same upstream**: refcount is incremented and the existing TCP
    ///   port is returned. Normal case (N containers sharing one workspace).
    /// - **Different upstream**: treat the existing entry as stale (the
    ///   reclaimed-from-state-file case where the user's `$SSH_AUTH_SOCK`
    ///   changed between daemon runs, or a `cella up --rebuild` after a
    ///   daemon bounce). Tear down the stale bridge and rebind. Prefer
    ///   the same port so any lingering container with the old baked
    ///   env var still resolves; if the port can't be re-grabbed in the
    ///   tight window after teardown, fall through to a fresh port.
    ///
    /// # Errors
    ///
    /// Returns `CellaDaemonError::Socket` if no TCP listener can be
    /// bound (extremely unlikely for `127.0.0.1:0`).
    ///
    /// # Panics
    ///
    /// Does not panic in normal operation. `expect("just indexed")`
    /// on the stale-upstream branch is proven unreachable by the
    /// preceding `HashMap::get` returning `Some`.
    pub async fn register(
        &mut self,
        workspace: PathBuf,
        upstream_socket: PathBuf,
    ) -> Result<u16, CellaDaemonError> {
        // Three cases: reuse (same upstream), replace (different upstream),
        // fresh (no entry). Compute once to avoid overlapping get/get_mut.
        let preferred_port = match self.bridges.get(&workspace) {
            Some(entry) if entry.upstream_socket == upstream_socket => {
                let port = entry.bridge_port;
                if let Some(e) = self.bridges.get_mut(&workspace) {
                    e.refcount += 1;
                }
                self.persist_state();
                return Ok(port);
            }
            Some(entry) => {
                // Stale upstream — tear down so we can rebind fresh.
                let preferred = entry.bridge_port;
                let removed = self.bridges.remove(&workspace).expect("just indexed");
                warn!(
                    workspace = %workspace.display(),
                    port = preferred,
                    old_upstream = %removed.upstream_socket.display(),
                    new_upstream = %upstream_socket.display(),
                    "ssh-agent bridge: upstream changed, recreating bridge"
                );
                let _ = removed.shutdown_tx.send(true);
                removed.accept_task.abort();
                Some(preferred)
            }
            None => None,
        };

        let listener = bind_preferred_or_random(preferred_port).await?;
        let bridge_port =
            listener
                .local_addr()
                .map(|a| a.port())
                .map_err(|e| CellaDaemonError::Socket {
                    message: format!("ssh-agent bridge: local_addr failed: {e}"),
                })?;

        debug!(
            workspace = %workspace.display(),
            port = bridge_port,
            upstream = %upstream_socket.display(),
            "ssh-agent bridge: bound TCP listener"
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let upstream_for_task = upstream_socket.clone();
        let auth_for_task = self.auth_token.clone();
        let task = tokio::spawn(async move {
            run_accept_loop(listener, upstream_for_task, auth_for_task, shutdown_rx).await;
        });

        let entry = BridgeEntry {
            upstream_socket,
            bridge_port,
            refcount: 1,
            accept_task: task.abort_handle(),
            shutdown_tx,
        };
        self.bridges.insert(workspace, entry);
        self.persist_state();
        Ok(bridge_port)
    }

    /// Decrement the refcount for `workspace`. When the refcount reaches
    /// zero, the accept task is aborted, in-flight bridges are signaled
    /// to close, and `true` is returned to indicate teardown happened.
    /// Returns `false` while the bridge is still in use or when the
    /// workspace was never registered.
    pub fn release(&mut self, workspace: &Path) -> bool {
        let Some(entry) = self.bridges.get_mut(workspace) else {
            return false;
        };
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount > 0 {
            self.persist_state();
            return false;
        }
        let Some(removed) = self.bridges.remove(workspace) else {
            return false;
        };
        // Signal in-flight bridges to drop their streams (which EOFs
        // both peers) BEFORE aborting the accept loop. send() returns
        // Err only when there are no receivers — we don't care.
        let _ = removed.shutdown_tx.send(true);
        removed.accept_task.abort();
        debug!(
            workspace = %workspace.display(),
            port = removed.bridge_port,
            "ssh-agent bridge: torn down"
        );
        self.persist_state();
        true
    }

    /// Lookup the upstream socket registered for `workspace`, if any.
    #[must_use]
    pub fn upstream_for(&self, workspace: &Path) -> Option<&Path> {
        self.bridges
            .get(workspace)
            .map(|e| e.upstream_socket.as_path())
    }

    /// Lookup the active refcount for `workspace`.
    #[must_use]
    pub fn refcount_for(&self, workspace: &Path) -> usize {
        self.bridges.get(workspace).map_or(0, |e| e.refcount)
    }

    /// Lookup the active bridge TCP port for `workspace`, if any.
    #[must_use]
    pub fn bridge_port_for(&self, workspace: &Path) -> Option<u16> {
        self.bridges.get(workspace).map(|e| e.bridge_port)
    }

    /// Path to the JSON snapshot of live bridge state.
    pub fn state_file_path(&self) -> PathBuf {
        self.state_dir.join("ssh-agent.state")
    }

    /// Serialize the current bridge registry to `ssh-agent.state` atomically:
    /// write to `<path>.tmp` first, then rename onto `<path>`. POSIX rename
    /// is atomic, so a daemon crash mid-write can never leave readers
    /// staring at a half-written file. Best-effort: serialization or
    /// filesystem failures log at warn and never propagate, so a busted
    /// filesystem can't take down register/release.
    fn persist_state(&self) {
        let bridges: Vec<serde_json::Value> = self
            .bridges
            .iter()
            .map(|(workspace, entry)| {
                serde_json::json!({
                    "workspace": workspace.to_string_lossy(),
                    "upstream_socket": entry.upstream_socket.to_string_lossy(),
                    "bridge_port": entry.bridge_port,
                    "refcount": entry.refcount,
                })
            })
            .collect();

        let snapshot = serde_json::json!({
            "schema_version": STATE_FILE_SCHEMA_VERSION,
            "daemon_pid": self.daemon_pid,
            "written_at_unix_sec": crate::shared::current_time_secs(),
            "bridges": bridges,
        });

        let path = self.state_file_path();
        let bytes = match serde_json::to_vec_pretty(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                warn!("ssh-agent bridge: state-file serialize failed: {e}");
                return;
            }
        };

        let tmp = path.with_extension("state.tmp");
        if let Err(e) = std::fs::write(&tmp, &bytes) {
            warn!(
                "ssh-agent bridge: state-file write {} failed: {e}",
                tmp.display()
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!(
                "ssh-agent bridge: state-file rename to {} failed: {e}",
                path.display()
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Schema version stamped into the persisted state file. Bump whenever the
/// JSON shape changes in a backward-incompatible way; readers should refuse
/// versions they don't understand.
pub const STATE_FILE_SCHEMA_VERSION: u32 = 2;

/// Stable, short hex hash of a workspace path.
///
/// Used to identify bridges in logs and the state file. Truncated
/// SHA-256 to 16 hex chars (64 bits) — collision risk is negligible
/// for the number of concurrent workspaces a user runs.
pub fn workspace_hash(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    let digest = hex::encode(hasher.finalize());
    digest[..16].to_string()
}

/// Try binding `127.0.0.1:<preferred>` first; if that fails (e.g. the
/// port just got released and the OS is still holding it in `TIME_WAIT`,
/// or another process grabbed it), fall through to a random port.
/// Returns the bound `TcpListener`; surfaces `CellaDaemonError::Socket`
/// only when neither attempt succeeds.
async fn bind_preferred_or_random(preferred: Option<u16>) -> Result<TcpListener, CellaDaemonError> {
    if let Some(port) = preferred
        && let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{port}")).await
    {
        return Ok(listener);
    }
    TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("ssh-agent bridge: bind 127.0.0.1:0 failed: {e}"),
        })
}

/// Accept TCP connections on `listener` and proxy each one to
/// `upstream_socket`. Loops until the listener errors, the task is
/// aborted, or `shutdown` fires. Each spawned per-connection bridge
/// inherits the same shutdown receiver so teardown propagates to
/// in-flight transfers.
async fn run_accept_loop(
    listener: TcpListener,
    upstream_socket: PathBuf,
    auth_token: String,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        let upstream = upstream_socket.clone();
                        let token = auth_token.clone();
                        let child_shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            proxy_one_connection(stream, upstream, token, child_shutdown).await;
                        });
                    }
                    Err(e) => {
                        warn!("ssh-agent bridge: accept failed, exiting loop: {e}");
                        return;
                    }
                }
            }
        }
    }
}

/// Bridge a single accepted client to a fresh upstream connection.
/// Reads a one-line auth-token handshake first, then bidirectionally
/// copies bytes until either side closes or `shutdown` fires.
///
/// The handshake is a single line containing only the daemon's
/// `auth_token`, then `\n`. This prevents arbitrary processes that
/// happen to find the loopback port from speaking ssh-agent through
/// the user's keys. After the handshake the stream is opaque bytes —
/// the proxy never parses ssh-agent protocol.
async fn proxy_one_connection(
    downstream: TcpStream,
    upstream_socket: PathBuf,
    auth_token: String,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }

    let (down_read, mut down_writer) = downstream.into_split();
    // BufReader owns its internal buffer; we must keep using it as the
    // read side of the copy because `read_line` may pull bytes past the
    // newline into that buffer (a client that sends "token\n<agent
    // bytes>" in a single packet is routine). Calling `into_inner()`
    // here and reading from the raw socket would drop those bytes and
    // corrupt the ssh-agent protocol stream.
    let mut down_reader = BufReader::new(down_read);

    // Read auth line. read_line stops at \n but leaves trailing bytes
    // in the BufReader's internal buffer, which the subsequent copy
    // will consume transparently.
    let mut auth_line = String::new();
    match down_reader.read_line(&mut auth_line).await {
        Ok(0) => {
            debug!("ssh-agent bridge: client closed before auth");
            return;
        }
        Ok(_) => {}
        Err(e) => {
            debug!("ssh-agent bridge: auth read failed: {e}");
            return;
        }
    }
    if auth_line.trim() != auth_token {
        warn!("ssh-agent bridge: rejecting connection with bad auth token");
        let _ = down_writer.shutdown().await;
        return;
    }

    let upstream = match UnixStream::connect(&upstream_socket).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                upstream = %upstream_socket.display(),
                "ssh-agent bridge: upstream connect failed: {e}"
            );
            return;
        }
    };
    let (up_read, up_write) = upstream.into_split();

    let down_to_up = async {
        let mut up_write = up_write;
        let _ = tokio::io::copy(&mut down_reader, &mut up_write).await;
        let _ = up_write.shutdown().await;
    };
    let up_to_down = async {
        let mut up_read = up_read;
        let _ = tokio::io::copy(&mut up_read, &mut down_writer).await;
        let _ = down_writer.shutdown().await;
    };

    tokio::select! {
        biased;
        _ = shutdown.changed() => {
            debug!("ssh-agent bridge: shutdown signaled, closing in-flight bridge");
        }
        () = async { tokio::join!(down_to_up, up_to_down); } => {}
    }
}

/// Construct a fresh shared manager.
#[must_use]
pub fn new_shared(state_dir: PathBuf, auth_token: String) -> SharedSshProxyManager {
    Arc::new(Mutex::new(SshProxyManager::new(state_dir, auth_token)))
}

impl SshProxyManager {
    /// Reclaim bridges from the persisted state file.
    ///
    /// Runs at daemon startup so containers created before the daemon
    /// bounced — whose `CELLA_SSH_AGENT_BRIDGE` env var is baked in at
    /// create time and points at a specific loopback port — keep
    /// working instead of hanging on a dead port. For each entry in
    /// the state file we try to `TcpListener::bind` on the exact same
    /// port; if it's still free (usually is, since the old daemon
    /// released it on shutdown), we spawn a fresh accept loop bridging
    /// to the original upstream socket. The in-container agent's
    /// baked-in `host.docker.internal:<port>` keeps resolving to a
    /// working bridge.
    ///
    /// Refcount is reset to 1 on reclaim — we have no authoritative
    /// way to tell how many containers are using the bridge after a
    /// restart. Subsequent register/release calls correct the count
    /// over time.
    ///
    /// Best-effort: ports that can't be reclaimed (already in use by
    /// another process, state-file corrupt, etc.) are logged and
    /// skipped; the affected containers will need `cella down &&
    /// cella up` to recover.
    pub async fn reclaim_from_state_file(&mut self) {
        let path = self.state_file_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!(
                    "ssh-agent bridge: state-file read {} failed: {e}",
                    path.display()
                );
                return;
            }
        };
        let snapshot: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "ssh-agent bridge: state-file parse {} failed: {e}",
                    path.display()
                );
                return;
            }
        };
        if snapshot["schema_version"].as_u64() != Some(u64::from(STATE_FILE_SCHEMA_VERSION)) {
            debug!(
                "ssh-agent bridge: state-file schema mismatch, skipping reclaim ({} vs {})",
                snapshot["schema_version"], STATE_FILE_SCHEMA_VERSION
            );
            return;
        }
        let Some(bridges) = snapshot["bridges"].as_array() else {
            return;
        };
        for entry in bridges {
            self.reclaim_one(entry).await;
        }
        self.persist_state();
    }

    async fn reclaim_one(&mut self, entry: &serde_json::Value) {
        let (Some(workspace), Some(upstream), Some(port)) = (
            entry["workspace"].as_str().map(PathBuf::from),
            entry["upstream_socket"].as_str().map(PathBuf::from),
            entry["bridge_port"]
                .as_u64()
                .and_then(|v| u16::try_from(v).ok()),
        ) else {
            warn!("ssh-agent bridge: state-file entry missing required fields, skipping");
            return;
        };

        let addr = format!("127.0.0.1:{port}");
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(
                    "ssh-agent bridge: could not reclaim port {port} for workspace {}: {e}",
                    workspace.display()
                );
                return;
            }
        };

        debug!(
            workspace = %workspace.display(),
            port,
            upstream = %upstream.display(),
            "ssh-agent bridge: reclaimed TCP listener from state file"
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let upstream_for_task = upstream.clone();
        let auth_for_task = self.auth_token.clone();
        let task = tokio::spawn(async move {
            run_accept_loop(listener, upstream_for_task, auth_for_task, shutdown_rx).await;
        });

        self.bridges.insert(
            workspace,
            BridgeEntry {
                upstream_socket: upstream,
                bridge_port: port,
                refcount: 1,
                accept_task: task.abort_handle(),
                shutdown_tx,
            },
        );
    }
}

/// Ensure the state directory exists.
///
/// Stale state-file is left in place (it'll be overwritten on the next
/// register/release). Stale Unix socket files from the old design are
/// unlinked here for a one-time cleanup.
///
/// # Errors
///
/// Returns `CellaDaemonError::Socket` if the directory cannot be created.
pub fn init_run_dir(run_dir: &Path) -> Result<(), CellaDaemonError> {
    std::fs::create_dir_all(run_dir).map_err(|e| CellaDaemonError::Socket {
        message: format!(
            "ssh-agent bridge: create dir {} failed: {e}",
            run_dir.display()
        ),
    })?;
    sweep_legacy_unix_sockets(run_dir);
    Ok(())
}

/// Unlink any `ssh-agent-*.sock` files in `run_dir` left behind by the
/// previous host-side Unix-socket design. Failures are logged at `warn`
/// and otherwise ignored — best-effort cleanup.
fn sweep_legacy_unix_sockets(run_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(run_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let ext_is_sock = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sock"));
        if name.starts_with("ssh-agent-") && ext_is_sock {
            match std::fs::remove_file(&path) {
                Ok(()) => debug!(
                    "ssh-agent bridge: swept legacy unix socket {}",
                    path.display()
                ),
                Err(e) => warn!(
                    "ssh-agent bridge: could not sweep legacy unix socket {}: {e}",
                    path.display()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    fn manager(run_dir: &Path) -> SshProxyManager {
        SshProxyManager::with_pid(run_dir.to_path_buf(), "test-token".to_string(), 7777)
    }

    /// Echo server on a Unix socket — stands in for the host's ssh-agent.
    fn spawn_echo_upstream(path: &Path) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).expect("bind upstream");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        })
    }

    /// Open a TCP connection to `port`, send the auth handshake, then
    /// return the (still-open) TCP stream.
    async fn connect_and_authenticate(port: u16, token: &str) -> TcpStream {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        sock.write_all(token.as_bytes()).await.unwrap();
        sock.write_all(b"\n").await.unwrap();
        sock
    }

    // ---------------------------------------------------------------
    // Pure data-structure tests.
    // ---------------------------------------------------------------

    #[test]
    fn workspace_hash_is_deterministic() {
        let a = workspace_hash(Path::new("/Users/me/proj"));
        let b = workspace_hash(Path::new("/Users/me/proj"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn workspace_hash_differs_per_path() {
        let a = workspace_hash(Path::new("/Users/me/proj-a"));
        let b = workspace_hash(Path::new("/Users/me/proj-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn release_unknown_workspace_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manager(dir.path());
        assert!(!m.release(Path::new("/never/registered")));
    }

    // ---------------------------------------------------------------
    // Refcount semantics with a real listener.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn register_returns_listening_tcp_port() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let port = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .await
            .expect("register");
        assert_ne!(port, 0);
        // Port must accept TCP connections.
        let _client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    }

    #[tokio::test]
    async fn register_twice_reuses_port_and_bumps_refcount() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");

        let first = m
            .register(workspace.clone(), upstream.clone())
            .await
            .unwrap();
        let second = m.register(workspace.clone(), upstream).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(m.refcount_for(&workspace), 2);
    }

    #[tokio::test]
    async fn release_decrements_without_teardown_when_still_used() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");

        m.register(workspace.clone(), upstream.clone())
            .await
            .unwrap();
        m.register(workspace.clone(), upstream).await.unwrap();

        assert!(!m.release(&workspace));
        assert_eq!(m.refcount_for(&workspace), 1);
    }

    #[tokio::test]
    async fn release_at_refcount_one_tears_down_listener() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");
        let port = m.register(workspace.clone(), upstream).await.unwrap();

        assert!(m.release(&workspace));
        assert_eq!(m.refcount_for(&workspace), 0);
        assert!(m.upstream_for(&workspace).is_none());

        // Listener is gone (give the runtime a tick to close).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }

    #[tokio::test]
    async fn register_with_same_upstream_bumps_refcount_without_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");

        let first_port = m
            .register(workspace.clone(), upstream.clone())
            .await
            .unwrap();
        let second_port = m.register(workspace.clone(), upstream).await.unwrap();

        assert_eq!(first_port, second_port);
        assert_eq!(m.refcount_for(&workspace), 2);
    }

    #[tokio::test]
    async fn register_with_new_upstream_replaces_stale_bridge() {
        // Regression: on a rebuild after daemon-restart reclaim, the
        // reclaimed entry's upstream may be stale (user rotated
        // $SSH_AUTH_SOCK). A fresh register must NOT just bump
        // refcount — that would forward bytes to the stale agent.
        // It must tear down and rebind with the current upstream.
        let dir = tempfile::tempdir().unwrap();
        let upstream_old = dir.path().join("upstream-old.sock");
        let upstream_new = dir.path().join("upstream-new.sock");
        let _up_old = spawn_echo_upstream(&upstream_old);
        // upstream_new is a distinct echo server so we can assert
        // traffic is flowing to IT, not the old one.
        let new_listener = UnixListener::bind(&upstream_new).unwrap();
        let new_marker = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let new_marker_for_task = new_marker.clone();
        let _up_new = tokio::spawn(async move {
            while let Ok((mut stream, _)) = new_listener.accept().await {
                let marker = new_marker_for_task.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        marker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");

        // First register pins the old upstream.
        let port_old = m.register(workspace.clone(), upstream_old).await.unwrap();

        // Second register (rebuild) with a different upstream: triggers
        // the teardown-and-rebind path. Refcount resets to 1 on
        // replacement — the reclaimed/stale entry's count is not
        // carried forward, because it may have been a lie.
        let port_new = m
            .register(workspace.clone(), upstream_new.clone())
            .await
            .unwrap();
        assert_eq!(
            m.refcount_for(&workspace),
            1,
            "rebuild must reset refcount, not carry stale count"
        );
        assert_eq!(
            m.upstream_for(&workspace),
            Some(upstream_new.as_path()),
            "upstream must be updated on replacement"
        );

        // Port preservation: best effort. Usually the same port rebinds
        // successfully because the OS releases it immediately after
        // shutdown on loopback. We only assert traffic goes to the NEW
        // upstream — which port it's on is a secondary concern.
        let _ = port_old; // silence clippy, we don't strictly need it.
        let mut client = connect_and_authenticate(port_new, "test-token").await;
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        assert!(
            new_marker.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "replaced bridge must forward bytes to the NEW upstream, not the stale one"
        );
    }

    #[tokio::test]
    async fn upstream_first_registration_wins() {
        let dir = tempfile::tempdir().unwrap();
        let upstream_a = dir.path().join("first.sock");
        let upstream_b = dir.path().join("second.sock");
        let _up_a = spawn_echo_upstream(&upstream_a);
        let _up_b = spawn_echo_upstream(&upstream_b);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");

        m.register(workspace.clone(), upstream_a.clone())
            .await
            .unwrap();
        m.register(workspace.clone(), upstream_b).await.unwrap();

        assert_eq!(m.upstream_for(&workspace), Some(upstream_a.as_path()));
    }

    // ---------------------------------------------------------------
    // End-to-end byte-fidelity tests through the bridge.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn bridge_forwards_bytes_after_valid_auth() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let port = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .await
            .unwrap();

        let mut client = connect_and_authenticate(port, "test-token").await;
        client.write_all(b"hello world").await.unwrap();
        let mut buf = vec![0u8; 11];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello world");
    }

    #[tokio::test]
    async fn bridge_does_not_drop_bytes_that_follow_auth_token_in_same_packet() {
        // Regression: the old implementation called BufReader::into_inner
        // after read_line, throwing away any bytes the BufReader had
        // already pulled past the newline. Real ssh-agent clients send
        // `token\n<request-bytes>` in a single packet — those request
        // bytes MUST reach the upstream agent. This test proves they do.
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let port = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .await
            .unwrap();

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // Send the token AND the payload in a single write — the OS
        // will almost certainly bundle them into one packet, which is
        // exactly the case where the bug manifests.
        client
            .write_all(b"test-token\nimmediate-payload")
            .await
            .unwrap();

        let mut buf = vec![0u8; 18];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(
            &buf, b"immediate-payload",
            "bytes sent in the same packet as the auth token must forward intact"
        );
    }

    #[tokio::test]
    async fn bridge_rejects_connection_with_bad_auth_token() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let port = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .await
            .unwrap();

        let mut client = connect_and_authenticate(port, "wrong-token").await;
        // Daemon shuts the connection immediately after reading the bad
        // auth line; read should EOF promptly.
        let mut buf = [0u8; 32];
        let n = client.read(&mut buf).await.unwrap_or(0);
        assert_eq!(n, 0, "expected EOF after bad auth, got {n} bytes");
    }

    #[tokio::test]
    async fn release_closes_in_flight_bridge_connections() {
        // Regression analog of the host-side Unix socket version: in-flight
        // bridges must EOF promptly after release, not hang forever.
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");
        let port = m.register(workspace.clone(), upstream).await.unwrap();

        let mut client = connect_and_authenticate(port, "test-token").await;
        client.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [b'x']);

        m.release(&workspace);

        let result = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
        let n = result
            .expect("bridge must EOF in-flight connections within 2s of release")
            .expect("read after release should not error");
        assert_eq!(n, 0, "expected EOF after teardown, got {n} bytes");
    }

    // ---------------------------------------------------------------
    // State-file persistence.
    // ---------------------------------------------------------------

    fn read_state(run_dir: &Path) -> serde_json::Value {
        let bytes = std::fs::read(run_dir.join("ssh-agent.state")).expect("state file missing");
        serde_json::from_slice(&bytes).expect("state file is valid JSON")
    }

    #[tokio::test]
    async fn state_file_written_on_register() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let port = m
            .register(PathBuf::from("/Users/me/proj"), upstream.clone())
            .await
            .unwrap();

        let snap = read_state(dir.path());
        assert_eq!(snap["schema_version"], STATE_FILE_SCHEMA_VERSION);
        assert_eq!(snap["daemon_pid"], 7777);
        let bridges = snap["bridges"].as_array().expect("bridges array");
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0]["workspace"], "/Users/me/proj");
        assert_eq!(bridges[0]["bridge_port"], port);
        assert_eq!(bridges[0]["refcount"], 1);
    }

    #[tokio::test]
    async fn state_file_drops_entry_on_full_release() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = manager(dir.path());
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), upstream).await.unwrap();
        m.release(&workspace);

        let snap = read_state(dir.path());
        assert_eq!(snap["bridges"].as_array().unwrap().len(), 0);
    }

    // ---------------------------------------------------------------
    // Run-dir initialization & legacy sweep.
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Reclaim from state file (daemon-restart recovery).
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn reclaim_rebinds_previously_registered_ports() {
        // Write a state file manually, then construct a fresh manager
        // and prove reclaim puts a working bridge on the recorded port.
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        // First, figure out an available port by binding then dropping.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // Hand-craft a state file that claims the bridge was running
        // for workspace /Users/me/proj on that port.
        let state = serde_json::json!({
            "schema_version": STATE_FILE_SCHEMA_VERSION,
            "daemon_pid": 0,
            "written_at_unix_sec": 0,
            "bridges": [{
                "workspace": "/Users/me/proj",
                "upstream_socket": upstream.to_string_lossy(),
                "bridge_port": port,
                "refcount": 1,
            }],
        });
        std::fs::write(
            dir.path().join("ssh-agent.state"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let mut m = manager(dir.path());
        m.reclaim_from_state_file().await;

        let workspace = PathBuf::from("/Users/me/proj");
        assert_eq!(m.bridge_port_for(&workspace), Some(port));
        assert_eq!(m.refcount_for(&workspace), 1);

        // The reclaimed listener accepts connections on the SAME port
        // — crucial for stopped containers whose baked-in env var
        // points here.
        let mut client = connect_and_authenticate(port, "test-token").await;
        client.write_all(b"roundtrip").await.unwrap();
        let mut buf = [0u8; 9];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"roundtrip");
    }

    #[tokio::test]
    async fn reclaim_skips_entries_whose_port_is_already_taken() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        // Bind a listener to hold the port — reclaim must skip gracefully.
        let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let held_port = held.local_addr().unwrap().port();

        let state = serde_json::json!({
            "schema_version": STATE_FILE_SCHEMA_VERSION,
            "daemon_pid": 0,
            "written_at_unix_sec": 0,
            "bridges": [{
                "workspace": "/Users/me/proj",
                "upstream_socket": upstream.to_string_lossy(),
                "bridge_port": held_port,
                "refcount": 1,
            }],
        });
        std::fs::write(
            dir.path().join("ssh-agent.state"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let mut m = manager(dir.path());
        m.reclaim_from_state_file().await;

        // Port was taken → no bridge reclaimed for that workspace.
        assert_eq!(
            m.refcount_for(&PathBuf::from("/Users/me/proj")),
            0,
            "reclaim must skip when port is already in use"
        );

        drop(held);
    }

    #[tokio::test]
    async fn reclaim_is_noop_when_state_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manager(dir.path());
        // Must not panic, must not create anything.
        m.reclaim_from_state_file().await;
        assert_eq!(m.refcount_for(Path::new("/x")), 0);
    }

    #[tokio::test]
    async fn reclaim_skips_state_file_with_wrong_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let state = serde_json::json!({
            "schema_version": 999,
            "daemon_pid": 0,
            "written_at_unix_sec": 0,
            "bridges": [{
                "workspace": "/Users/me/proj",
                "upstream_socket": "/nonexistent.sock",
                "bridge_port": 12345,
                "refcount": 1,
            }],
        });
        std::fs::write(
            dir.path().join("ssh-agent.state"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let mut m = manager(dir.path());
        m.reclaim_from_state_file().await;
        assert_eq!(m.refcount_for(Path::new("/Users/me/proj")), 0);
    }

    #[test]
    fn init_run_dir_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        assert!(!run.exists());
        init_run_dir(&run).unwrap();
        assert!(run.is_dir());
    }

    #[test]
    fn init_run_dir_sweeps_legacy_unix_socket_files() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        std::fs::create_dir(&run).unwrap();
        let stale = run.join("ssh-agent-deadbeefcafef00d.sock");
        std::fs::write(&stale, b"junk").unwrap();
        assert!(stale.exists());

        init_run_dir(&run).unwrap();
        assert!(!stale.exists(), "legacy ssh-agent socket must be unlinked");
    }

    #[test]
    fn init_run_dir_leaves_unrelated_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        std::fs::create_dir(&run).unwrap();

        let unrelated = run.join("daemon.token");
        std::fs::write(&unrelated, b"keep me").unwrap();
        let prefix_only = run.join("ssh-agent-without-suffix");
        std::fs::write(&prefix_only, b"keep me too").unwrap();

        init_run_dir(&run).unwrap();
        assert!(unrelated.exists());
        assert!(prefix_only.exists());
    }
}
