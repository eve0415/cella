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
use tokio::sync::Mutex;

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
    /// If a proxy already exists for `workspace`, the refcount is incremented
    /// and the existing proxy socket path is returned. The `upstream_socket`
    /// argument is honored only on the first registration; subsequent calls
    /// reuse the original upstream and ignore the new value.
    pub fn register(&mut self, workspace: PathBuf, upstream_socket: PathBuf) -> PathBuf {
        if let Some(entry) = self.proxies.get_mut(&workspace) {
            entry.refcount += 1;
            return entry.proxy_socket.clone();
        }

        let proxy_socket = self.proxy_socket_path(&workspace);
        let entry = ProxyEntry {
            upstream_socket,
            proxy_socket: proxy_socket.clone(),
            refcount: 1,
        };
        self.proxies.insert(workspace, entry);
        proxy_socket
    }

    /// Decrement the refcount for `workspace`. Returns the proxy socket path
    /// when the refcount reaches zero (so the caller can tear down the bound
    /// listener and unlink the socket file); returns `None` while the proxy
    /// is still in use.
    pub fn release(&mut self, workspace: &Path) -> Option<PathBuf> {
        let entry = self.proxies.get_mut(workspace)?;
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount == 0 {
            let removed = self.proxies.remove(workspace)?;
            Some(removed.proxy_socket)
        } else {
            None
        }
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

/// Construct a fresh shared manager.
#[must_use]
pub fn new_shared(run_dir: PathBuf) -> SharedSshProxyManager {
    Arc::new(Mutex::new(SshProxyManager::new(run_dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> SshProxyManager {
        SshProxyManager::new(PathBuf::from("/tmp/test-cella-run"))
    }

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
    fn register_returns_socket_under_run_dir() {
        let mut m = manager();
        let path = m.register(
            PathBuf::from("/Users/me/proj"),
            PathBuf::from("/host/agent.sock"),
        );
        assert!(path.starts_with("/tmp/test-cella-run"));
        assert!(path.to_string_lossy().contains("ssh-agent-"));
        assert!(path.to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn register_twice_reuses_socket_and_bumps_refcount() {
        let mut m = manager();
        let workspace = PathBuf::from("/Users/me/proj");
        let upstream = PathBuf::from("/host/agent.sock");

        let first = m.register(workspace.clone(), upstream.clone());
        let second = m.register(workspace.clone(), upstream);

        assert_eq!(first, second);
        assert_eq!(m.refcount_for(&workspace), 2);
    }

    #[test]
    fn release_decrements_refcount_without_teardown_when_still_used() {
        let mut m = manager();
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), PathBuf::from("/host/agent.sock"));
        m.register(workspace.clone(), PathBuf::from("/host/agent.sock"));

        let teardown = m.release(&workspace);
        assert!(teardown.is_none(), "still ref'd, must not return teardown");
        assert_eq!(m.refcount_for(&workspace), 1);
    }

    #[test]
    fn release_at_refcount_one_returns_socket_and_removes_entry() {
        let mut m = manager();
        let workspace = PathBuf::from("/Users/me/proj");
        let proxy = m.register(workspace.clone(), PathBuf::from("/host/agent.sock"));

        let teardown = m.release(&workspace);
        assert_eq!(teardown.as_deref(), Some(proxy.as_path()));
        assert_eq!(m.refcount_for(&workspace), 0);
        assert!(m.upstream_for(&workspace).is_none());
    }

    #[test]
    fn release_unknown_workspace_returns_none() {
        let mut m = manager();
        assert!(m.release(Path::new("/never/registered")).is_none());
    }

    #[test]
    fn release_does_not_underflow_when_called_too_many_times() {
        let mut m = manager();
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), PathBuf::from("/host/agent.sock"));
        m.release(&workspace);
        // Second release after teardown is a no-op (entry already gone).
        let again = m.release(&workspace);
        assert!(again.is_none());
    }

    #[test]
    fn upstream_first_registration_wins() {
        let mut m = manager();
        let workspace = PathBuf::from("/Users/me/proj");
        m.register(workspace.clone(), PathBuf::from("/host/first.sock"));
        m.register(workspace.clone(), PathBuf::from("/host/second.sock"));
        assert_eq!(
            m.upstream_for(&workspace),
            Some(Path::new("/host/first.sock"))
        );
    }

    #[test]
    fn distinct_workspaces_get_distinct_sockets() {
        let mut m = manager();
        let a = m.register(
            PathBuf::from("/Users/me/proj-a"),
            PathBuf::from("/host/agent.sock"),
        );
        let b = m.register(
            PathBuf::from("/Users/me/proj-b"),
            PathBuf::from("/host/agent.sock"),
        );
        assert_ne!(a, b);
    }
}
