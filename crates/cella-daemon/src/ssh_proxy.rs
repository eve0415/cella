//! Per-workspace SSH-agent proxy.
//!
//! Bridges the host's `$SSH_AUTH_SOCK` into a path under `~/.cella/run/` that
//! cella mounts into containers on colima. On colima the VM-side magic socket
//! `/run/host-services/ssh-auth.sock` is created by lima's OpenSSH agent
//! forwarding, which silently degenerates with sandboxed agents (1Password)
//! and can route to a connectable-but-empty agent. Owning the bridge here
//! removes that fragility — the daemon runs in the user's macOS context and
//! has full access to the real agent socket regardless of sandboxing.
//!
//! Lifecycle: refcounted per workspace folder. First `cella up` for a
//! workspace creates the proxy socket; subsequent ups for the same workspace
//! reuse it. The socket is unlinked when the refcount reaches zero.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

use crate::CellaDaemonError;

/// Shared proxy manager state.
pub type SharedSshProxyManager = Arc<Mutex<SshProxyManager>>;

/// Refcount-keyed registry of per-workspace SSH-agent proxies.
pub struct SshProxyManager {
    run_dir: PathBuf,
    proxies: HashMap<PathBuf, ProxyEntry>,
}

struct ProxyEntry {
    upstream_socket: PathBuf,
    proxy_socket: PathBuf,
    refcount: usize,
    accept_task: AbortHandle,
}

impl SshProxyManager {
    /// Create a new manager that places proxy sockets under `run_dir`.
    #[must_use]
    pub fn new(run_dir: PathBuf) -> Self {
        Self {
            run_dir,
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
            return Ok(entry.proxy_socket.clone());
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

        let upstream_for_task = upstream_socket.clone();
        let task = tokio::spawn(async move { run_accept_loop(listener, upstream_for_task).await });

        let entry = ProxyEntry {
            upstream_socket,
            proxy_socket: proxy_socket.clone(),
            refcount: 1,
            accept_task: task.abort_handle(),
        };
        self.proxies.insert(workspace, entry);
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
            return None;
        }
        let removed = self.proxies.remove(workspace)?;
        removed.accept_task.abort();
        let _ = std::fs::remove_file(&removed.proxy_socket);
        debug!(
            workspace = %workspace.display(),
            proxy = %removed.proxy_socket.display(),
            "ssh-agent proxy: torn down"
        );
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
}

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
/// Loops until the listener errors or the task is aborted.
async fn run_accept_loop(listener: UnixListener, upstream_socket: PathBuf) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let upstream = upstream_socket.clone();
                tokio::spawn(async move { proxy_one_connection(stream, upstream).await });
            }
            Err(e) => {
                warn!("ssh-agent proxy: accept failed, exiting loop: {e}");
                break;
            }
        }
    }
}

/// Bridge a single accepted client to a fresh upstream connection. Bytes
/// flow until either side closes. SSH-agent protocol is opaque to us; this
/// is pure bidirectional copy.
async fn proxy_one_connection(mut downstream: UnixStream, upstream_socket: PathBuf) {
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
    if let Err(e) = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await {
        debug!("ssh-agent proxy: connection ended with: {e}");
    }
}

/// Construct a fresh shared manager.
#[must_use]
pub fn new_shared(run_dir: PathBuf) -> SharedSshProxyManager {
    Arc::new(Mutex::new(SshProxyManager::new(run_dir)))
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
