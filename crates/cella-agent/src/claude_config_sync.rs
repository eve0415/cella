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
//! The shared `last_hash` records what each half last wrote or sent: the writer
//! updates it after a successful write, the watcher only after a successful send
//! (so a failed send isn't mistaken for already-synced). A matching hash marks
//! the agent's own write and is dropped, preventing a loop. On (re)connect the
//! agent also re-announces the current file (see [`reannounce_message`]).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cella_port::CellaPortError;
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
pub fn config_path() -> Option<PathBuf> {
    resolve_config_path(
        std::env::var("CELLA_CLAUDE_JSON_PATH").ok(),
        std::env::var("HOME").ok(),
    )
}

/// Read the current config file as a `ClaudeConfigChanged` to re-announce to the
/// daemon on (re)connect.
///
/// Returns `None` when `path` is `None` (sync disabled) or the file is
/// unreadable/absent. The caller sends this *before* starting the connection
/// reader, so the read happens before any inbound daemon push can clobber the
/// file — letting the daemon merge this container's state and push back anything
/// it is missing, instead of a stale push overwriting local edits.
pub async fn reannounce_message(path: Option<&Path>) -> Option<AgentMessage> {
    let content = tokio::fs::read_to_string(path?).await.ok()?;
    Some(AgentMessage::ClaudeConfigChanged { content })
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
fn initial_hash(path: &Path) -> String {
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
fn restore_owner(path: &Path) {
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
fn restore_owner(_path: &Path) {}

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

    let control = &control;
    while handle.changes.recv().await.is_some() {
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue; // mid-rename or transiently unreadable; next event covers it
        };
        forward_change(&last_hash, content, |msg| async move {
            control.lock().await.send(&msg).await
        })
        .await;
    }
}

/// Forward a container-side edit to the daemon, advancing `last_hash` only on a
/// successful send.
///
/// Recording the hash *after* a successful send (not before) is the fix for a
/// silent data-loss bug: if the daemon is unreachable, a failed send leaves
/// `last_hash` unchanged so the edit is re-sent on the next watcher event or on
/// reconnect, instead of being marked already-synced and later clobbered by a
/// stale daemon push. Content whose hash already matches `last_hash` is the
/// agent's own (daemon-applied) write and is skipped without sending.
async fn forward_change<F, Fut>(last_hash: &SharedHash, content: String, send: F)
where
    F: FnOnce(AgentMessage) -> Fut,
    Fut: Future<Output = Result<(), CellaPortError>>,
{
    let hash = cella_filesync::sha256_hex(content.as_bytes());
    if *last_hash.lock().await == hash {
        return; // our own (daemon-applied) write — don't echo it back
    }
    match send(AgentMessage::ClaudeConfigChanged { content }).await {
        Ok(()) => *last_hash.lock().await = hash,
        Err(e) => warn!("claude sync: failed to send config change to daemon: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forward_change_keeps_hash_when_send_fails() {
        // The fix for silent offline-edit loss: a failed send must NOT advance
        // the hash, so the edit stays eligible for re-send on the next event or
        // on reconnect (rather than being marked synced and later clobbered).
        let last_hash: SharedHash = Arc::new(Mutex::new(String::new()));
        forward_change(&last_hash, r#"{"a":1}"#.to_string(), |_msg| async {
            Err(CellaPortError::ControlSocket {
                message: "daemon down".to_string(),
            })
        })
        .await;
        assert!(
            last_hash.lock().await.is_empty(),
            "a failed send must not advance last_hash"
        );
    }

    #[tokio::test]
    async fn forward_change_advances_hash_on_success() {
        let last_hash: SharedHash = Arc::new(Mutex::new(String::new()));
        forward_change(&last_hash, r#"{"a":1}"#.to_string(), |_msg| async {
            Ok(())
        })
        .await;
        assert_eq!(
            *last_hash.lock().await,
            cella_filesync::sha256_hex(br#"{"a":1}"#)
        );
    }

    #[tokio::test]
    async fn forward_change_skips_unchanged_content() {
        // Content whose hash already matches is the agent's own daemon-applied
        // write; it must not be sent back, preventing an echo loop.
        use std::sync::atomic::{AtomicBool, Ordering};
        let content = r#"{"a":1}"#;
        let hash = cella_filesync::sha256_hex(content.as_bytes());
        let last_hash: SharedHash = Arc::new(Mutex::new(hash.clone()));
        let sent = Arc::new(AtomicBool::new(false));
        let sent_in_closure = sent.clone();
        forward_change(&last_hash, content.to_string(), |_msg| {
            sent_in_closure.store(true, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await;
        assert!(
            !sent.load(Ordering::SeqCst),
            "matching content must not be sent"
        );
        assert_eq!(*last_hash.lock().await, hash, "hash must stay unchanged");
    }

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

    #[tokio::test]
    async fn reannounce_message_reads_current_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, r#"{"a":1}"#).unwrap();
        let Some(AgentMessage::ClaudeConfigChanged { content }) =
            reannounce_message(Some(&path)).await
        else {
            panic!("expected a ClaudeConfigChanged re-announce");
        };
        assert_eq!(content, r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn reannounce_message_none_without_path() {
        assert!(reannounce_message(None).await.is_none());
    }

    #[tokio::test]
    async fn reannounce_message_none_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(reannounce_message(Some(&path)).await.is_none());
    }
}
