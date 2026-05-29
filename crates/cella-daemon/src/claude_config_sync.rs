//! Host-side hub for bidirectional `~/.claude.json` sync.
//!
//! Holds the canonical config plus a content hash, deep-merges host and
//! container edits (via [`cella_env::claude_code::merge_claude_config`]), and
//! drives broadcasts to opted-in agents.
//!
//! Loop suppression hashes the *raw bytes* the daemon last wrote to / observed
//! on the host file: an inbound watcher event whose bytes hash to `last_hash`
//! is the daemon's own write and is dropped. The originating container is
//! excluded from re-broadcast so a container edit never echoes back to itself.
//!
//! Known, accepted limitations (see crate/docs): key deletions don't propagate,
//! and concurrent edits to the same scalar resolve last-writer-wins.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use cella_protocol::DaemonMessage;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::control_server::ContainerHandle;

/// Shared registry of connected containers (mirrors `ControlContext`).
type Handles = Arc<Mutex<HashMap<String, ContainerHandle>>>;

/// Canonical `~/.claude.json` state held by the daemon.
pub struct ClaudeSyncState {
    /// Merged canonical config — the union of the host file and every
    /// opted-in container's edits.
    canonical: serde_json::Value,
    /// SHA-256 of the raw bytes last written to / observed on the host file.
    last_hash: String,
}

impl ClaudeSyncState {
    /// Seed from the host file at startup. An absent or malformed file yields
    /// an empty object so merges still work.
    #[must_use]
    pub fn load(path: Option<&Path>) -> Self {
        let raw = path.and_then(|p| std::fs::read(p).ok());
        let canonical = raw
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let last_hash = raw
            .as_deref()
            .map(cella_filesync::sha256_hex)
            .unwrap_or_default();
        Self {
            canonical,
            last_hash,
        }
    }

    /// Pretty-printed canonical config for transport and host writes.
    fn canonical_string(&self) -> String {
        serde_json::to_string_pretty(&self.canonical).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Handle a host-side change to `~/.claude.json` detected by the watcher.
///
/// Reads the file, drops the event if it is the daemon's own write, deep-merges
/// the host content into the canonical config, writes the merged result back
/// when the host file is missing container-only keys, and broadcasts the new
/// canonical to opted-in agents when it actually changed.
pub async fn on_host_change(
    state: &Arc<Mutex<ClaudeSyncState>>,
    handles: &Handles,
    host_path: &Path,
) {
    let Ok(raw) = std::fs::read(host_path) else {
        debug!("claude sync: host file unreadable (mid-rename?); waiting for next event");
        return;
    };

    let incoming_hash = cella_filesync::sha256_hex(&raw);
    {
        let mut st = state.lock().await;
        if incoming_hash == st.last_hash {
            return; // our own write, or already processed
        }
        st.last_hash = incoming_hash;
    }

    let Ok(incoming) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        warn!("claude sync: host ~/.claude.json is not valid JSON; skipping");
        return;
    };

    let (out, host_needs_update, canonical_changed) = {
        let mut st = state.lock().await;
        let merged = cella_env::claude_code::merge_claude_config(&st.canonical, &incoming);
        let host_needs_update = merged != incoming; // host file lacks container-only keys
        let canonical_changed = merged != st.canonical;
        st.canonical = merged;
        (st.canonical_string(), host_needs_update, canonical_changed)
    };

    if host_needs_update {
        write_host_guarded(state, host_path, &out).await;
    }

    if canonical_changed {
        broadcast(handles, &out, None).await;
    }
}

/// Write `out` to the host file, recording its hash as `last_hash` *before*
/// writing so the self-triggered watcher event is recognised as our own and
/// dropped. This guard ordering is the load-bearing loop-suppression invariant,
/// so both write sites share this single helper.
async fn write_host_guarded(state: &Arc<Mutex<ClaudeSyncState>>, path: &Path, out: &str) {
    state.lock().await.last_hash = cella_filesync::sha256_hex(out.as_bytes());
    if let Err(e) = cella_filesync::atomic_write(path, out.as_bytes(), 0o600) {
        warn!("claude sync: failed to write host ~/.claude.json: {e}");
    }
}

/// Handle an inbound `ClaudeConfigChanged` from the container named `sender`.
///
/// Deep-merges the container content into the canonical config, writes it to
/// the host file, and re-broadcasts to every *other* opted-in agent.
pub async fn on_agent_change(
    state: &Arc<Mutex<ClaudeSyncState>>,
    handles: &Handles,
    host_path: Option<&Path>,
    content: &str,
    sender: &str,
) {
    let Ok(incoming) = serde_json::from_str::<serde_json::Value>(content) else {
        warn!("claude sync: container {sender} sent invalid ~/.claude.json; skipping");
        return;
    };

    let (out, canonical_changed) = {
        let mut st = state.lock().await;
        let merged = cella_env::claude_code::merge_claude_config(&st.canonical, &incoming);
        let canonical_changed = merged != st.canonical;
        st.canonical = merged;
        (st.canonical_string(), canonical_changed)
    };

    if !canonical_changed {
        return; // nothing new from this container
    }

    if let Some(path) = host_path {
        write_host_guarded(state, path, &out).await;
    }

    broadcast(handles, &out, Some(sender)).await;
}

/// Push the current canonical config to a single just-connected agent.
pub async fn push_current(state: &Arc<Mutex<ClaudeSyncState>>) -> DaemonMessage {
    let content = state.lock().await.canonical_string();
    DaemonMessage::SyncClaudeConfig { content }
}

/// Send `content` as a `SyncClaudeConfig` to every opted-in connected agent,
/// optionally excluding one container (the origin of an inbound change).
async fn broadcast(handles: &Handles, content: &str, exclude: Option<&str>) {
    // Clone the senders under the lock, then send after releasing it — never
    // hold the registry mutex across an await.
    let senders: Vec<tokio::sync::mpsc::Sender<DaemonMessage>> = {
        let registry = handles.lock().await;
        registry
            .iter()
            .filter(|(name, h)| h.claude_config_sync && exclude != Some(name.as_str()))
            .filter_map(|(_, h)| h.agent_tx.clone())
            .collect()
    };

    for tx in senders {
        let _ = tx
            .send(DaemonMessage::SyncClaudeConfig {
                content: content.to_string(),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_from(json: serde_json::Value) -> Arc<Mutex<ClaudeSyncState>> {
        let bytes = serde_json::to_vec(&json).unwrap();
        Arc::new(Mutex::new(ClaudeSyncState {
            canonical: json,
            last_hash: cella_filesync::sha256_hex(&bytes),
        }))
    }

    #[test]
    fn load_absent_file_is_empty_object() {
        let st = ClaudeSyncState::load(None);
        assert_eq!(st.canonical, serde_json::json!({}));
        assert!(st.last_hash.is_empty());
    }

    #[tokio::test]
    async fn on_agent_change_merges_and_preserves_host_projects() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        // Canonical/host start with a host-namespaced project.
        let initial = serde_json::json!({
            "projects": { "/Users/eve/p": { "k": 1 } }
        });
        std::fs::write(&host, serde_json::to_vec_pretty(&initial).unwrap()).unwrap();
        let state = state_from(initial);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // Container sends its own (disjoint) project namespace.
        let container = serde_json::json!({
            "projects": { "/workspaces/p": { "k": 2 } }
        });
        on_agent_change(
            &state,
            &handles,
            Some(&host),
            &serde_json::to_string(&container).unwrap(),
            "cella-test",
        )
        .await;

        // Host file now contains BOTH project namespaces (deep-merge union).
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host).unwrap()).unwrap();
        assert_eq!(written["projects"]["/Users/eve/p"]["k"], 1);
        assert_eq!(written["projects"]["/workspaces/p"]["k"], 2);
    }

    #[tokio::test]
    async fn on_agent_change_ignores_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        std::fs::write(&host, b"{}").unwrap();
        let state = state_from(serde_json::json!({}));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        on_agent_change(&state, &handles, Some(&host), "not json", "cella-test").await;
        // Host file untouched, no panic.
        assert_eq!(std::fs::read(&host).unwrap(), b"{}");
    }
}
