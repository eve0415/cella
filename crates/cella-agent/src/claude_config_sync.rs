//! In-container side of `~/.claude.json` bidirectional sync.
//!
//! Two halves sharing a single content hash for loop suppression:
//! - **watcher** (container → daemon): on a debounced change to
//!   `$HOME/.claude.json`, reads the file and sends [`AgentMessage::
//!   ClaudeConfigChanged`] to the daemon — unless the content matches the last
//!   hash (i.e. it is a write the agent itself just applied).
//! - **writer** (daemon → container): applies inbound `SyncClaudeConfig`
//!   content via an atomic `0o600` write — unless it matches the last hash.
//!
//! The shared `last_hash` is updated *before* writing so the resulting watcher
//! event is recognised as the agent's own and dropped, preventing a sync loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cella_protocol::AgentMessage;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use crate::reconnecting_client::ReconnectingClient;

/// Debounce for coalescing rapid editor writes.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Hash of the content last written to / read from the container file, shared
/// by both halves to suppress feedback loops.
type SharedHash = Arc<Mutex<String>>;

/// Whether this agent participates in `~/.claude.json` sync, per the
/// `CELLA_SYNC_CLAUDE_CONFIG` env var set by the orchestrator at create time.
pub fn sync_enabled() -> bool {
    std::env::var("CELLA_SYNC_CLAUDE_CONFIG").as_deref() == Ok("1")
}

/// The exact `~/.claude.json` path to sync (reads process env).
fn config_path() -> Option<PathBuf> {
    resolve_config_path(
        std::env::var("CELLA_CLAUDE_JSON_PATH").ok(),
        std::env::var("HOME").ok(),
    )
}

/// Resolve the path to sync from the pinned env var and `$HOME`.
///
/// Prefers `pinned` (`CELLA_CLAUDE_JSON_PATH`, set by the orchestrator to the
/// remote user's home) so the agent and the seed agree even when the agent
/// daemon runs as a different user than `remote_user`. Falls back to
/// `$HOME/.claude.json`. Empty strings are treated as unset.
fn resolve_config_path(pinned: Option<String>, home: Option<String>) -> Option<PathBuf> {
    if let Some(p) = pinned.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".claude.json"))
}

/// Spawn the watcher and writer tasks. `apply_rx` receives canonical content
/// pushed by the daemon (via the control reader's `SyncClaudeConfig` arm).
pub fn spawn(control: Arc<Mutex<ReconnectingClient>>, apply_rx: mpsc::Receiver<String>) {
    let Some(path) = config_path() else {
        warn!("claude sync: cannot resolve config path ($HOME unset); sync disabled");
        return;
    };
    let last_hash: SharedHash = Arc::new(Mutex::new(initial_hash(&path)));

    tokio::spawn(run_writer(path.clone(), apply_rx, last_hash.clone()));
    tokio::spawn(run_watcher(path, control, last_hash));
}

/// Hash of the file's current content, or empty if it doesn't exist yet.
fn initial_hash(path: &std::path::Path) -> String {
    std::fs::read(path)
        .ok()
        .map(|b| cella_filesync::sha256_hex(&b))
        .unwrap_or_default()
}

/// Apply daemon-pushed config to the container file (daemon → container).
async fn run_writer(path: PathBuf, mut apply_rx: mpsc::Receiver<String>, last_hash: SharedHash) {
    while let Some(content) = apply_rx.recv().await {
        let hash = cella_filesync::sha256_hex(content.as_bytes());
        if *last_hash.lock().await == hash {
            continue; // already have this content
        }
        match cella_filesync::atomic_write(&path, content.as_bytes(), 0o600) {
            Ok(()) => {
                restore_owner(&path);
                // Record the hash only after a successful write so a transient
                // failure doesn't suppress the retry of identical content.
                *last_hash.lock().await = hash;
                debug!("claude sync: applied daemon update to {}", path.display());
            }
            Err(e) => warn!("claude sync: failed to write {}: {e}", path.display()),
        }
    }
}

/// Restore the config file's ownership to whoever owns its parent directory.
///
/// The agent daemon usually runs as root, but `atomic_write` (temp + rename)
/// installs a fresh root-owned inode — which the remote user's `claude` then
/// can't read. Chowning to the home directory's owner (best-effort, no-op when
/// the agent already runs as that user) keeps the config readable.
#[cfg(unix)]
fn restore_owner(path: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else { return };
    let Ok(meta) = std::fs::metadata(parent) else {
        return;
    };
    if let Err(e) = std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid())) {
        warn!(
            "claude sync: could not restore owner on {}: {e}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn restore_owner(_path: &std::path::Path) {}

/// Watch the container file and forward edits to the daemon (container → daemon).
async fn run_watcher(
    path: PathBuf,
    control: Arc<Mutex<ReconnectingClient>>,
    last_hash: SharedHash,
) {
    let mut handle = match cella_filesync::watch_file(&path, DEBOUNCE) {
        Ok(h) => h,
        Err(e) => {
            warn!("claude sync: cannot watch {}: {e}", path.display());
            return;
        }
    };
    debug!("claude sync: watching {}", path.display());

    while handle.changes.recv().await.is_some() {
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue; // mid-rename or transiently unreadable; next event covers it
        };
        let hash = cella_filesync::sha256_hex(content.as_bytes());
        {
            let mut lh = last_hash.lock().await;
            if *lh == hash {
                continue; // our own (daemon-applied) write — don't echo it back
            }
            *lh = hash;
        }
        if let Err(e) = control
            .lock()
            .await
            .send(&AgentMessage::ClaudeConfigChanged { content })
            .await
        {
            warn!("claude sync: failed to send config change to daemon: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_config_path_prefers_pinned_over_home() {
        // The pinned path wins even when $HOME points elsewhere (agent running
        // as root vs remote_user vscode) — this is the bug guard.
        let got = resolve_config_path(
            Some("/home/vscode/.claude.json".to_string()),
            Some("/root".to_string()),
        );
        assert_eq!(got, Some(PathBuf::from("/home/vscode/.claude.json")));
    }

    #[test]
    fn resolve_config_path_falls_back_to_home() {
        let got = resolve_config_path(None, Some("/home/vscode".to_string()));
        assert_eq!(got, Some(PathBuf::from("/home/vscode/.claude.json")));
    }

    #[test]
    fn resolve_config_path_treats_empty_as_unset() {
        let got = resolve_config_path(Some(String::new()), Some("/home/vscode".to_string()));
        assert_eq!(got, Some(PathBuf::from("/home/vscode/.claude.json")));
        assert_eq!(resolve_config_path(None, Some(String::new())), None);
        assert_eq!(resolve_config_path(None, None), None);
    }

    #[test]
    fn initial_hash_empty_for_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(initial_hash(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn initial_hash_matches_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, b"{\"a\":1}").unwrap();
        assert_eq!(
            initial_hash(&path),
            cella_filesync::sha256_hex(b"{\"a\":1}")
        );
    }

    #[tokio::test]
    async fn writer_applies_inbound_content_and_records_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let (tx, rx) = mpsc::channel(4);
        let last_hash: SharedHash = Arc::new(Mutex::new(String::new()));
        let task = tokio::spawn(run_writer(path.clone(), rx, last_hash.clone()));

        tx.send(r#"{"a":1}"#.to_string()).await.unwrap();
        drop(tx); // close channel so the writer loop exits
        task.await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"a":1}"#);
        assert_eq!(
            *last_hash.lock().await,
            cella_filesync::sha256_hex(br#"{"a":1}"#)
        );
    }

    #[tokio::test]
    async fn writer_skips_when_hash_already_matches() {
        // Guards the loop: content the agent already has (same hash) is not
        // re-written, so a daemon echo of our own edit is a no-op.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let content = r#"{"a":1}"#;
        let (tx, rx) = mpsc::channel(4);
        let last_hash: SharedHash =
            Arc::new(Mutex::new(cella_filesync::sha256_hex(content.as_bytes())));
        let task = tokio::spawn(run_writer(path.clone(), rx, last_hash));

        tx.send(content.to_string()).await.unwrap();
        drop(tx);
        task.await.unwrap();

        assert!(
            !path.exists(),
            "writer must skip a write whose hash already matches"
        );
    }
}
