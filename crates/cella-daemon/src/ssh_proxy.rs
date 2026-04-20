//! Per-workspace SSH-agent proxy.
//!
//! ## ⚠️ DESIGN STATUS: BROKEN ON COLIMA — DO NOT WIRE UP IN PRODUCTION
//!
//! This module was built to fix `git commit -S` failing inside cella-managed
//! containers on colima with sandboxed agents (1Password). The design:
//! daemon on the macOS host accepts on a Unix socket under `~/.cella/run/`,
//! bridges bytes back to the real `$SSH_AUTH_SOCK`, container bind-mounts
//! that socket. Empirical Phase 1 probe **failed**:
//!
//! ```text
//! docker: Error response from daemon: error while creating mount source
//!   path '/Users/u/.cella/run/cella-virtiofs-probe.sock':
//!   mkdir <path>: operation not supported
//! ```
//!
//! Colima virtiofs rejects `mkdir` for **any** Unix-socket path created on
//! the macOS host — not just the macOS sandbox dirs we originally suspected.
//! Docker's mkdir-source-if-missing fallback hits this on every bind-mount
//! attempt, killing the mount before it begins. A daemon-on-host proxy
//! cannot work through this restriction.
//!
//! The two failure modes the proxy was meant to fix are still real:
//!
//! 1. Lima's `forwardAgent` (`/run/host-services/ssh-auth.sock`) silently
//!    degenerates with sandboxed agents — connectable-but-empty.
//! 2. Direct bind-mount of `$SSH_AUTH_SOCK` (especially in a macOS sandbox
//!    dir) fails at docker mkdir-source.
//!
//! But the proxy, as built here, also fails at docker mkdir-source on the
//! same virtiofs restriction. The fix has to live INSIDE the colima VM
//! (e.g. a helper spawned via `colima ssh` that creates the socket VM-side
//! and bridges to the host over the existing SSH tunnel — same shape as
//! VS Code Remote-SSH's `/tmp/vscode-ssh-auth-<uuid>.sock`, where the
//! `/tmp` is the colima VM's `/tmp`, not the macOS host's).
//!
//! Code is left in place as a documented spike. The orchestrator wiring
//! routes colima through this module via `RegisterSshAgentProxy`, which
//! will fail with the mkdir-EOPNOTSUPP error at `cella up` mount time.
//! Until a VM-side helper is in place, the existing `cella down && cella
//! up` recovery does not work; users on colima with sandboxed agents
//! should use `OrbStack` or Docker Desktop instead.
//!
//! Lifecycle: refcounted per workspace folder. First `cella up` for a
//! workspace creates the proxy socket; subsequent ups for the same workspace
//! reuse it. The socket is unlinked when the refcount reaches zero.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, watch};
use tokio::task::AbortHandle;
use tracing::{debug, warn};

use crate::CellaDaemonError;

/// Shared proxy manager state.
pub type SharedSshProxyManager = Arc<Mutex<SshProxyManager>>;

/// Refcount-keyed registry of per-workspace SSH-agent proxies.
pub struct SshProxyManager {
    run_dir: PathBuf,
    daemon_pid: u32,
    proxies: HashMap<PathBuf, ProxyEntry>,
}

struct ProxyEntry {
    upstream_socket: PathBuf,
    proxy_socket: PathBuf,
    refcount: usize,
    accept_task: AbortHandle,
    /// Broadcast channel that signals teardown to the accept loop and to
    /// every spawned per-connection bridge. Sending `true` causes the
    /// accept loop and any in-flight bridges to drop their streams, which
    /// closes the corresponding fds and EOFs both peers.
    shutdown_tx: watch::Sender<bool>,
}

impl SshProxyManager {
    /// Create a new manager that places proxy sockets under `run_dir`. The
    /// `daemon_pid` is recorded in the persisted state file so external
    /// readers (e.g. a future `cella doctor` probe) can verify liveness.
    #[must_use]
    pub fn new(run_dir: PathBuf) -> Self {
        Self::with_pid(run_dir, std::process::id())
    }

    /// Construct a manager with an explicit `daemon_pid`. Tests use this so
    /// state-file assertions are stable across runs.
    #[must_use]
    pub fn with_pid(run_dir: PathBuf, daemon_pid: u32) -> Self {
        Self {
            run_dir,
            daemon_pid,
            proxies: HashMap::new(),
        }
    }

    /// Register a proxy for `workspace` bridging to `upstream_socket`. Returns
    /// the proxy socket path that the caller should bind-mount into the
    /// container.
    ///
    /// On first registration this binds a `UnixListener` at the proxy socket
    /// path, sets it to mode 0o600, and spawns an accept-loop task that
    /// forwards each accepted connection to `upstream_socket`. If a proxy
    /// already exists for `workspace`, the refcount is incremented, the
    /// existing accept task continues, and the original proxy-socket path is
    /// returned. The `upstream_socket` argument is honored only on the first
    /// registration; subsequent calls reuse the original upstream.
    ///
    /// # Errors
    ///
    /// Returns `CellaDaemonError::Socket` if the listener cannot be bound
    /// (e.g. the parent directory does not exist or another process already
    /// holds the path).
    pub fn register(
        &mut self,
        workspace: PathBuf,
        upstream_socket: PathBuf,
    ) -> Result<PathBuf, CellaDaemonError> {
        if let Some(entry) = self.proxies.get_mut(&workspace) {
            entry.refcount += 1;
            let path = entry.proxy_socket.clone();
            self.persist_state();
            return Ok(path);
        }

        let proxy_socket = self.proxy_socket_path(&workspace);
        // Best-effort cleanup of a stale socket file from a previous run.
        let _ = std::fs::remove_file(&proxy_socket);

        let listener = UnixListener::bind(&proxy_socket).map_err(|e| CellaDaemonError::Socket {
            message: format!(
                "ssh-agent proxy: bind {} failed: {e}",
                proxy_socket.display()
            ),
        })?;
        crate::shared::set_socket_permissions(&proxy_socket);

        debug!(
            workspace = %workspace.display(),
            proxy = %proxy_socket.display(),
            upstream = %upstream_socket.display(),
            "ssh-agent proxy: bound listener"
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let upstream_for_task = upstream_socket.clone();
        let task = tokio::spawn(async move {
            run_accept_loop(listener, upstream_for_task, shutdown_rx).await;
        });

        let entry = ProxyEntry {
            upstream_socket,
            proxy_socket: proxy_socket.clone(),
            refcount: 1,
            accept_task: task.abort_handle(),
            shutdown_tx,
        };
        self.proxies.insert(workspace, entry);
        self.persist_state();
        Ok(proxy_socket)
    }

    /// Decrement the refcount for `workspace`. When the refcount reaches zero,
    /// the accept task is aborted, the proxy socket file is unlinked, and the
    /// path that was torn down is returned. Returns `None` while the proxy is
    /// still in use or when the workspace was never registered.
    pub fn release(&mut self, workspace: &Path) -> Option<PathBuf> {
        let entry = self.proxies.get_mut(workspace)?;
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount > 0 {
            self.persist_state();
            return None;
        }
        let removed = self.proxies.remove(workspace)?;
        // Signal in-flight bridges to drop their streams (which EOFs both
        // peers) BEFORE aborting the accept loop. send() returns Err only
        // when there are no receivers — we don't care.
        let _ = removed.shutdown_tx.send(true);
        removed.accept_task.abort();
        let _ = std::fs::remove_file(&removed.proxy_socket);
        debug!(
            workspace = %workspace.display(),
            proxy = %removed.proxy_socket.display(),
            "ssh-agent proxy: torn down"
        );
        self.persist_state();
        Some(removed.proxy_socket)
    }

    /// Lookup the upstream socket registered for `workspace`, if any.
    #[must_use]
    pub fn upstream_for(&self, workspace: &Path) -> Option<&Path> {
        self.proxies
            .get(workspace)
            .map(|e| e.upstream_socket.as_path())
    }

    /// Lookup the active refcount for `workspace`.
    #[must_use]
    pub fn refcount_for(&self, workspace: &Path) -> usize {
        self.proxies.get(workspace).map_or(0, |e| e.refcount)
    }

    fn proxy_socket_path(&self, workspace: &Path) -> PathBuf {
        let hash = workspace_hash(workspace);
        self.run_dir.join(format!("ssh-agent-{hash}.sock"))
    }

    /// Path to the JSON snapshot of live proxy state.
    pub fn state_file_path(&self) -> PathBuf {
        self.run_dir.join("ssh-agent.state")
    }

    /// Serialize the current proxy registry to `ssh-agent.state` atomically:
    /// write to `<path>.tmp` first, then rename onto `<path>`. POSIX rename
    /// is atomic, so a daemon crash mid-write can never leave readers staring
    /// at a half-written file. Best-effort: serialization or filesystem
    /// failures log at warn and never propagate, so a busted filesystem can't
    /// take down register/release.
    fn persist_state(&self) {
        let proxies: Vec<serde_json::Value> = self
            .proxies
            .iter()
            .map(|(workspace, entry)| {
                serde_json::json!({
                    "workspace": workspace.to_string_lossy(),
                    "upstream_socket": entry.upstream_socket.to_string_lossy(),
                    "proxy_socket": entry.proxy_socket.to_string_lossy(),
                    "refcount": entry.refcount,
                })
            })
            .collect();

        let snapshot = serde_json::json!({
            "schema_version": STATE_FILE_SCHEMA_VERSION,
            "daemon_pid": self.daemon_pid,
            "written_at_unix_sec": crate::shared::current_time_secs(),
            "proxies": proxies,
        });

        let path = self.state_file_path();
        let bytes = match serde_json::to_vec_pretty(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                warn!("ssh-agent proxy: state-file serialize failed: {e}");
                return;
            }
        };

        let tmp = path.with_extension("state.tmp");
        if let Err(e) = std::fs::write(&tmp, &bytes) {
            warn!(
                "ssh-agent proxy: state-file write {} failed: {e}",
                tmp.display()
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!(
                "ssh-agent proxy: state-file rename to {} failed: {e}",
                path.display()
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Schema version stamped into the persisted state file. Bump whenever the
/// JSON shape changes in a backward-incompatible way; readers should refuse
/// versions they don't understand.
pub const STATE_FILE_SCHEMA_VERSION: u32 = 1;

/// Stable, short hex hash of a workspace path used to build the proxy socket
/// filename. Truncated SHA-256 to 16 hex chars (64 bits) — collision risk is
/// negligible for the number of concurrent workspaces a user runs.
fn workspace_hash(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    let digest = hex::encode(hasher.finalize());
    digest[..16].to_string()
}

/// Accept connections on `listener` and proxy each one to `upstream_socket`.
/// Loops until the listener errors, the task is aborted, or `shutdown` fires.
/// Each spawned per-connection bridge inherits the same shutdown receiver
/// so teardown propagates to in-flight transfers.
async fn run_accept_loop(
    listener: UnixListener,
    upstream_socket: PathBuf,
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
                        let child_shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            proxy_one_connection(stream, upstream, child_shutdown).await;
                        });
                    }
                    Err(e) => {
                        warn!("ssh-agent proxy: accept failed, exiting loop: {e}");
                        return;
                    }
                }
            }
        }
    }
}

/// Bridge a single accepted client to a fresh upstream connection. Bytes
/// flow until either side closes or `shutdown` fires. When shutdown wins,
/// `downstream` and `upstream` are dropped, which closes their fds and
/// EOFs both peers — SSH-agent clients inside the container observe this
/// as a connection close, not as a hung-on-read.
async fn proxy_one_connection(
    mut downstream: UnixStream,
    upstream_socket: PathBuf,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }
    let mut upstream = match UnixStream::connect(&upstream_socket).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                upstream = %upstream_socket.display(),
                "ssh-agent proxy: upstream connect failed: {e}"
            );
            return;
        }
    };
    tokio::select! {
        biased;
        _ = shutdown.changed() => {
            debug!("ssh-agent proxy: shutdown signaled, closing in-flight bridge");
        }
        res = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => {
            if let Err(e) = res {
                debug!("ssh-agent proxy: connection ended with: {e}");
            }
        }
    }
}

/// Construct a fresh shared manager.
#[must_use]
pub fn new_shared(run_dir: PathBuf) -> SharedSshProxyManager {
    Arc::new(Mutex::new(SshProxyManager::new(run_dir)))
}

/// Ensure the SSH-proxy run directory exists and is free of stale sockets
/// from previous daemon runs.
///
/// Creates `run_dir` (recursively) if missing, then unlinks any
/// `ssh-agent-*.sock` files left behind by a daemon that exited without
/// teardown — a fresh daemon owns no proxies, so any pre-existing sockets
/// are guaranteed stale and would refuse `bind` on a future register.
///
/// # Errors
///
/// Returns `CellaDaemonError::Socket` if the directory cannot be created.
pub fn init_run_dir(run_dir: &Path) -> Result<(), CellaDaemonError> {
    std::fs::create_dir_all(run_dir).map_err(|e| CellaDaemonError::Socket {
        message: format!(
            "ssh-agent proxy: create dir {} failed: {e}",
            run_dir.display()
        ),
    })?;
    sweep_stale_sockets(run_dir);
    Ok(())
}

/// Unlink any `ssh-agent-*.sock` files in `run_dir`. Failures are logged at
/// `warn` and otherwise ignored — best-effort cleanup.
fn sweep_stale_sockets(run_dir: &Path) {
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
                Ok(()) => debug!("ssh-agent proxy: swept stale socket {}", path.display()),
                Err(e) => warn!(
                    "ssh-agent proxy: could not sweep stale socket {}: {e}",
                    path.display()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// Spawn a mock upstream agent socket that, for every accepted connection,
    /// reads bytes and writes them back (echo server). Returns a join handle
    /// whose drop has no effect — the listener is owned by the spawned task
    /// and lives until the test runtime is torn down.
    fn spawn_echo_upstream(path: &Path) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).expect("bind upstream");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
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

    // -----------------------------------------------------------------
    // Pure data-structure tests (no async runtime needed).
    // -----------------------------------------------------------------

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
    fn release_unknown_workspace_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        assert!(m.release(Path::new("/never/registered")).is_none());
    }

    // -----------------------------------------------------------------
    // Refcount semantics with a real listener (each test uses a fresh
    // tempdir for run_dir + upstream socket).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn register_returns_socket_under_run_dir_and_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let path = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .expect("register");

        assert!(path.starts_with(dir.path()));
        assert!(path.to_string_lossy().contains("ssh-agent-"));
        assert!(path.to_string_lossy().ends_with(".sock"));
        assert!(path.exists(), "proxy socket file must exist after bind");
    }

    #[tokio::test]
    async fn register_twice_reuses_socket_and_bumps_refcount() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let workspace = PathBuf::from("/Users/me/proj");

        let first = m.register(workspace.clone(), upstream.clone()).unwrap();
        let second = m.register(workspace.clone(), upstream).unwrap();

        assert_eq!(first, second);
        assert_eq!(m.refcount_for(&workspace), 2);
    }

    #[tokio::test]
    async fn release_decrements_without_teardown_when_still_used() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let workspace = PathBuf::from("/Users/me/proj");

        m.register(workspace.clone(), upstream.clone()).unwrap();
        m.register(workspace.clone(), upstream).unwrap();

        let teardown = m.release(&workspace);
        assert!(teardown.is_none(), "still ref'd, must not return teardown");
        assert_eq!(m.refcount_for(&workspace), 1);
    }

    #[tokio::test]
    async fn release_at_refcount_one_unlinks_socket_and_clears_entry() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let workspace = PathBuf::from("/Users/me/proj");
        let proxy = m.register(workspace.clone(), upstream).unwrap();

        let teardown = m.release(&workspace);
        assert_eq!(teardown.as_deref(), Some(proxy.as_path()));
        assert_eq!(m.refcount_for(&workspace), 0);
        assert!(m.upstream_for(&workspace).is_none());
        assert!(!proxy.exists(), "socket file must be unlinked on teardown");
    }

    #[tokio::test]
    async fn upstream_first_registration_wins() {
        let dir = tempfile::tempdir().unwrap();
        let upstream_a = dir.path().join("first.sock");
        let upstream_b = dir.path().join("second.sock");
        let _up_a = spawn_echo_upstream(&upstream_a);
        let _up_b = spawn_echo_upstream(&upstream_b);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let workspace = PathBuf::from("/Users/me/proj");

        m.register(workspace.clone(), upstream_a.clone()).unwrap();
        m.register(workspace.clone(), upstream_b).unwrap();

        assert_eq!(m.upstream_for(&workspace), Some(upstream_a.as_path()));
    }

    #[tokio::test]
    async fn distinct_workspaces_get_distinct_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let a = m
            .register(PathBuf::from("/Users/me/proj-a"), upstream.clone())
            .unwrap();
        let b = m
            .register(PathBuf::from("/Users/me/proj-b"), upstream)
            .unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn bind_failure_surfaces_socket_error_when_run_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        // Point run_dir at a missing subdirectory so bind() must fail.
        let mut m = SshProxyManager::new(dir.path().join("missing"));
        let err = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .expect_err("expected bind failure");
        assert!(matches!(err, CellaDaemonError::Socket { .. }));
    }

    #[tokio::test]
    async fn register_clears_stale_socket_file_from_previous_run() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        // Pre-create a stale file at the would-be proxy path so register has
        // to unlink it before re-binding.
        let workspace = PathBuf::from("/Users/me/proj");
        let stale = dir
            .path()
            .join(format!("ssh-agent-{}.sock", workspace_hash(&workspace)));
        std::fs::write(&stale, b"junk").unwrap();
        assert!(stale.exists());

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let proxy = m.register(workspace, upstream).expect("register");
        assert_eq!(proxy, stale);
        assert!(proxy.exists());
    }

    // -----------------------------------------------------------------
    // End-to-end byte-fidelity tests through the actual bridge.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn bridge_forwards_bytes_to_upstream_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let proxy = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .unwrap();

        let mut client = UnixStream::connect(&proxy).await.expect("client connect");
        client.write_all(b"hello world").await.unwrap();

        let mut buf = vec![0u8; 11];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello world");
    }

    #[tokio::test]
    async fn bridge_handles_multiple_concurrent_connections() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let proxy = m
            .register(PathBuf::from("/Users/me/proj"), upstream)
            .unwrap();

        // Spawn three concurrent clients; each sends a distinct payload and
        // expects to receive the same payload back.
        let (tx, mut rx) = mpsc::channel::<()>(3);
        for i in 0..3u8 {
            let proxy = proxy.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut client = UnixStream::connect(&proxy).await.unwrap();
                let payload = vec![i; 64];
                client.write_all(&payload).await.unwrap();
                let mut buf = vec![0u8; 64];
                client.read_exact(&mut buf).await.unwrap();
                assert_eq!(buf, payload);
                let _ = tx.send(()).await;
            });
        }
        drop(tx);

        // Wait for all three to report success.
        for _ in 0..3 {
            rx.recv().await.expect("client task completed");
        }
    }

    // -----------------------------------------------------------------
    // Run-dir initialization & stale-socket sweep.
    // -----------------------------------------------------------------

    #[test]
    fn init_run_dir_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        assert!(!run.exists());
        init_run_dir(&run).unwrap();
        assert!(run.is_dir());
    }

    #[test]
    fn init_run_dir_is_idempotent_when_dir_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("run")).unwrap();
        init_run_dir(&dir.path().join("run")).unwrap();
    }

    #[test]
    fn init_run_dir_sweeps_existing_ssh_agent_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        std::fs::create_dir(&run).unwrap();
        let stale = run.join("ssh-agent-deadbeefcafef00d.sock");
        std::fs::write(&stale, b"junk").unwrap();
        assert!(stale.exists());

        init_run_dir(&run).unwrap();
        assert!(!stale.exists(), "stale ssh-agent socket must be unlinked");
    }

    // -----------------------------------------------------------------
    // State-file persistence.
    // -----------------------------------------------------------------

    fn read_state(run_dir: &Path) -> serde_json::Value {
        let bytes = std::fs::read(run_dir.join("ssh-agent.state")).expect("state file missing");
        serde_json::from_slice(&bytes).expect("state file is valid JSON")
    }

    #[tokio::test]
    async fn state_file_written_on_register() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::with_pid(dir.path().to_path_buf(), 7777);
        m.register(PathBuf::from("/Users/me/proj"), upstream.clone())
            .unwrap();

        let snap = read_state(dir.path());
        assert_eq!(snap["schema_version"], STATE_FILE_SCHEMA_VERSION);
        assert_eq!(snap["daemon_pid"], 7777);
        let proxies = snap["proxies"].as_array().expect("proxies array");
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0]["workspace"], "/Users/me/proj");
        assert_eq!(
            proxies[0]["upstream_socket"],
            serde_json::Value::String(upstream.to_string_lossy().into_owned())
        );
        assert_eq!(proxies[0]["refcount"], 1);
    }

    #[tokio::test]
    async fn state_file_write_does_not_leave_tmp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::with_pid(dir.path().to_path_buf(), 1);
        m.register(PathBuf::from("/Users/me/proj"), upstream)
            .unwrap();

        let tmp = dir.path().join("ssh-agent.state.tmp");
        assert!(!tmp.exists(), "tmp file must be renamed away, not stranded");
    }

    #[tokio::test]
    async fn state_file_reflects_refcount_growth() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::with_pid(dir.path().to_path_buf(), 1);
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), upstream.clone()).unwrap();
        m.register(workspace, upstream).unwrap();

        let snap = read_state(dir.path());
        assert_eq!(snap["proxies"][0]["refcount"], 2);
    }

    #[tokio::test]
    async fn state_file_drops_entry_on_full_release() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::with_pid(dir.path().to_path_buf(), 1);
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), upstream).unwrap();
        m.release(&workspace);

        let snap = read_state(dir.path());
        assert_eq!(snap["proxies"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn state_file_decrements_on_partial_release() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::with_pid(dir.path().to_path_buf(), 1);
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), upstream.clone()).unwrap();
        m.register(workspace.clone(), upstream).unwrap();
        m.release(&workspace);

        let snap = read_state(dir.path());
        assert_eq!(snap["proxies"][0]["refcount"], 1);
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
        let suffix_only = run.join("other.sock");
        std::fs::write(&suffix_only, b"keep me three").unwrap();

        init_run_dir(&run).unwrap();
        assert!(unrelated.exists());
        assert!(prefix_only.exists());
        assert!(suffix_only.exists());
    }

    #[tokio::test]
    async fn release_closes_in_flight_bridge_connections() {
        // Regression: prior to per-connection shutdown propagation, release
        // aborted only the accept loop and unlinked the socket file, leaving
        // already-accepted bridge tasks running until the client side
        // disconnected. SSH-agent clients in the container would hang on
        // read indefinitely. After the fix, release must close in-flight
        // bridges so peers observe EOF promptly.
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let workspace = PathBuf::from("/Users/me/proj");
        let proxy = m.register(workspace.clone(), upstream).unwrap();

        // Open a client and round-trip a byte to prove the bridge is live.
        let mut client = UnixStream::connect(&proxy).await.unwrap();
        client.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [b'x']);

        // Release while the client connection is still open and idle.
        m.release(&workspace);

        // The proxy must close our connection. read() should return 0
        // (EOF) within a couple seconds, not hang forever.
        let read_result = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
        let n = read_result
            .expect("bridge must EOF in-flight connections within 2s of release")
            .expect("read after release should not error");
        assert_eq!(n, 0, "expected EOF (read=0) after teardown, got {n} bytes");
    }

    #[tokio::test]
    async fn release_makes_subsequent_connect_fail() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream.sock");
        let _up = spawn_echo_upstream(&upstream);

        let mut m = SshProxyManager::new(dir.path().to_path_buf());
        let workspace = PathBuf::from("/Users/me/proj");
        let proxy = m.register(workspace.clone(), upstream).unwrap();

        // Confirm the proxy works while live.
        let mut client = UnixStream::connect(&proxy).await.unwrap();
        client.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [b'x']);
        drop(client);

        // Tear down and verify the socket is gone, so connect now errors.
        m.release(&workspace);
        assert!(!proxy.exists());
        let connect_err = UnixStream::connect(&proxy).await;
        assert!(connect_err.is_err());
    }
}
