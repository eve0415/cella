//! Host-side hub for bidirectional `~/.claude.json` sync.
//!
//! Holds the canonical config, a content hash, and a per-source snapshot of the
//! last content seen from the host and each container. A change is turned into an
//! RFC 7386 merge-patch by diffing it against that source's snapshot (so a
//! removed key becomes an explicit `null`), then applied to the canonical config
//! — which is written to the host file and broadcast to opted-in agents. This is
//! how key *deletions* propagate to the host and every other container.
//!
//! Loop suppression hashes the *raw bytes* the daemon last wrote to / observed
//! on the host file: an inbound watcher event whose bytes hash to `last_hash`
//! is the daemon's own write and is dropped. The originating container is
//! excluded from re-broadcast so a container edit never echoes back to itself;
//! it is instead pushed the full canonical only when it is missing keys.
//!
//! Accepted limitation: concurrent edits to the same scalar resolve
//! last-writer-wins.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use cella_protocol::DaemonMessage;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::control_server::ContainerHandle;

/// Shared registry of connected containers (mirrors `ControlContext`).
type Handles = Arc<Mutex<HashMap<String, ContainerHandle>>>;

/// Snapshot key for the host source (container names key their own snapshots).
const HOST: &str = "<host>";

/// Canonical `~/.claude.json` state held by the daemon.
pub struct ClaudeSyncState {
    /// Merged canonical config — the union of the host file and every
    /// opted-in container's edits.
    canonical: serde_json::Value,
    /// SHA-256 of the raw bytes last written to / observed on the host file.
    last_hash: String,
    /// Last content observed *from* each source (the [`HOST`] sentinel or a
    /// container name), diffed against the next change to derive adds, changes,
    /// and deletions. Set only from content read from a source (or the canonical
    /// the daemon itself wrote to the host) — never from a push, since an
    /// unapplied push must not let a later diff fabricate deletions.
    snapshots: HashMap<String, serde_json::Value>,
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
        let mut snapshots = HashMap::new();
        snapshots.insert(HOST.to_string(), canonical.clone());
        Self {
            canonical,
            last_hash,
            snapshots,
        }
    }

    /// Pretty-printed canonical config for transport and host writes.
    fn canonical_string(&self) -> String {
        serde_json::to_string_pretty(&self.canonical).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Handle a host-side change to `~/.claude.json` detected by the watcher.
///
/// Reads the file, drops the event if it is the daemon's own write, then diffs
/// the host content against the last-seen host snapshot to derive a merge-patch
/// (including deletions) and applies it to the canonical config. Writes the
/// merged result back when the host file is missing container-only keys, and
/// broadcasts the new canonical to opted-in agents when it actually changed.
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
        let empty = serde_json::json!({});
        let prev = st.snapshots.get(HOST).unwrap_or(&empty);
        let patch = cella_env::claude_code::diff_merge_patch(prev, &incoming);
        let merged = cella_env::claude_code::apply_merge_patch(&st.canonical, &patch);
        let canonical_changed = merged != st.canonical;
        st.canonical = merged;
        // Canonical may still hold container-only keys the host file lacks; write
        // them back so the host file converges to the union.
        let host_needs_update = st.canonical != incoming;
        st.snapshots.insert(HOST.to_string(), incoming);
        (st.canonical_string(), host_needs_update, canonical_changed)
    };

    if host_needs_update {
        write_host_guarded(state, host_path, &out).await;
    }

    if canonical_changed {
        broadcast(handles, &out, None).await;
    }
}

/// Write `out` to the host file, recording its hash as `last_hash` only on a
/// successful write. The recorded hash lets the self-triggered watcher event be
/// recognised as the daemon's own write and dropped; the watcher debounce is far
/// longer than a write+hash, so the hash is in place before the event arrives.
/// Recording it only on success means a failed write never leaves the daemon
/// believing stale content is on disk. Both write sites share this helper.
async fn write_host_guarded(state: &Arc<Mutex<ClaudeSyncState>>, path: &Path, out: &str) {
    match cella_filesync::atomic_write(path, out.as_bytes(), 0o600) {
        Ok(()) => {
            let hash = cella_filesync::sha256_hex(out.as_bytes());
            let written = serde_json::from_str::<serde_json::Value>(out)
                .unwrap_or_else(|_| serde_json::json!({}));
            let mut st = state.lock().await;
            st.last_hash = hash;
            // The host file now equals `out`; record it as the host snapshot so a
            // later host edit diffs against what's actually on disk.
            st.snapshots.insert(HOST.to_string(), written);
        }
        Err(e) => warn!("claude sync: failed to write host ~/.claude.json: {e}"),
    }
}

/// Handle an inbound `ClaudeConfigChanged` from the container named `sender`.
///
/// Diffs the container content against the sender's last-seen snapshot to derive
/// a merge-patch (including deletions) and applies it to the canonical config.
/// When the canonical changes, writes it to the host file and re-broadcasts to
/// every *other* opted-in agent. Separately, when the merged canonical holds keys
/// the sender lacks (a reconnect, a missed broadcast, or fresh peer state), pushes
/// the canonical back to the sender so it converges — without echoing every
/// steady-state edit back to its origin.
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

    let (out, canonical_changed, needs_pushback) = {
        let mut st = state.lock().await;
        let empty = serde_json::json!({});
        let prev = st.snapshots.get(sender).unwrap_or(&empty);
        let patch = cella_env::claude_code::diff_merge_patch(prev, &incoming);
        let merged = cella_env::claude_code::apply_merge_patch(&st.canonical, &patch);
        let canonical_changed = merged != st.canonical;
        st.canonical = merged;
        // `apply` makes canonical a superset of `incoming`, so `!=` means the
        // sender is missing some canonical key and needs the full config back.
        let needs_pushback = st.canonical != incoming;
        st.snapshots.insert(sender.to_string(), incoming);
        (st.canonical_string(), canonical_changed, needs_pushback)
    };

    if canonical_changed {
        if let Some(path) = host_path {
            write_host_guarded(state, path, &out).await;
        }
        broadcast(handles, &out, Some(sender)).await;
    }

    if needs_pushback {
        send_to(handles, sender, &out).await;
    }
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

/// Send `content` as a `SyncClaudeConfig` to a single opted-in agent by name.
/// Used to converge a sender that is missing canonical keys (reconnect/catch-up).
async fn send_to(handles: &Handles, name: &str, content: &str) {
    let tx = {
        let registry = handles.lock().await;
        registry
            .get(name)
            .filter(|h| h.claude_config_sync)
            .and_then(|h| h.agent_tx.clone())
    };
    if let Some(tx) = tx {
        let _ = tx
            .send(DaemonMessage::SyncClaudeConfig {
                content: content.to_string(),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn state_from(json: serde_json::Value) -> Arc<Mutex<ClaudeSyncState>> {
        let bytes = serde_json::to_vec(&json).unwrap();
        let mut snapshots = HashMap::new();
        snapshots.insert(HOST.to_string(), json.clone());
        Arc::new(Mutex::new(ClaudeSyncState {
            canonical: json,
            last_hash: cella_filesync::sha256_hex(&bytes),
            snapshots,
        }))
    }

    /// A registered opted-in container whose daemon-pushed messages are captured.
    fn register_agent(handles: &Handles, name: &str) -> tokio::sync::mpsc::Receiver<DaemonMessage> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = ContainerHandle {
            container_id: name.to_string(),
            agent_state: Arc::new(crate::control_server::AgentConnectionState::new()),
            backend_kind: None,
            docker_host: None,
            agent_tx: Some(tx),
            claude_config_sync: true,
            agent_tx_generation: 0,
        };
        handles
            .try_lock()
            .expect("uncontended in test")
            .insert(name.to_string(), handle);
        rx
    }

    #[test]
    fn load_absent_file_is_empty_object() {
        let st = ClaudeSyncState::load(None);
        assert_eq!(st.canonical, json!({}));
        assert!(st.last_hash.is_empty());
    }

    #[tokio::test]
    async fn on_agent_change_merges_and_preserves_host_projects() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        // Canonical/host start with a host-namespaced project.
        let initial = json!({
            "projects": { "/Users/eve/p": { "k": 1 } }
        });
        std::fs::write(&host, serde_json::to_vec_pretty(&initial).unwrap()).unwrap();
        let state = state_from(initial);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // Container sends its own (disjoint) project namespace.
        let container = json!({
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
    async fn write_host_guarded_keeps_hash_when_write_fails() {
        // On a write failure the hash must NOT advance — otherwise the daemon
        // believes the (never-written) content is the host's on-disk state, and
        // a restart would re-seed from a stale file.
        let state = state_from(json!({ "a": 1 }));
        let before = state.lock().await.last_hash.clone();
        // A path whose parent directory does not exist makes atomic_write fail.
        let bad = Path::new("/nonexistent-cella-xyz/.claude.json");
        write_host_guarded(&state, bad, r#"{"a":2}"#).await;
        assert_eq!(
            state.lock().await.last_hash,
            before,
            "a failed host write must not advance last_hash"
        );
    }

    #[tokio::test]
    async fn write_host_guarded_advances_hash_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        let state = state_from(json!({ "a": 1 }));
        write_host_guarded(&state, &host, r#"{"a":2}"#).await;
        assert_eq!(
            state.lock().await.last_hash,
            cella_filesync::sha256_hex(br#"{"a":2}"#)
        );
        assert_eq!(std::fs::read_to_string(&host).unwrap(), r#"{"a":2}"#);
    }

    #[tokio::test]
    async fn on_agent_change_ignores_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        std::fs::write(&host, b"{}").unwrap();
        let state = state_from(json!({}));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        on_agent_change(&state, &handles, Some(&host), "not json", "cella-test").await;
        // Host file untouched, no panic.
        assert_eq!(std::fs::read(&host).unwrap(), b"{}");
    }

    #[tokio::test]
    async fn on_host_change_propagates_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        let initial = json!({ "mcpServers": { "s": 1 }, "keep": true });
        std::fs::write(&host, serde_json::to_vec(&initial).unwrap()).unwrap();
        let state = state_from(initial);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // User removes mcpServers on the host.
        std::fs::write(&host, serde_json::to_vec(&json!({ "keep": true })).unwrap()).unwrap();
        on_host_change(&state, &handles, &host).await;

        assert_eq!(
            state.lock().await.canonical,
            json!({ "keep": true }),
            "a host-side deletion must drop the key from canonical"
        );
    }

    #[tokio::test]
    async fn on_host_change_preserves_container_only_keys() {
        // A host edit must not delete keys the host never had (container-only).
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join(".claude.json");
        let host_view = json!({ "keep": true });
        std::fs::write(&host, serde_json::to_vec(&host_view).unwrap()).unwrap();
        let state = state_from(json!({
            "keep": true,
            "projects": { "/workspaces/p": { "k": 2 } }
        }));
        // Snapshot[HOST] reflects what the host last had (no container key).
        state
            .lock()
            .await
            .snapshots
            .insert(HOST.to_string(), host_view);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // Host adds a key; the container-only project must survive.
        std::fs::write(
            &host,
            serde_json::to_vec(&json!({ "keep": true, "theme": "dark" })).unwrap(),
        )
        .unwrap();
        on_host_change(&state, &handles, &host).await;

        let canon = state.lock().await.canonical.clone();
        assert_eq!(canon["projects"]["/workspaces/p"]["k"], 2);
        assert_eq!(canon["theme"], "dark");
    }

    #[tokio::test]
    async fn on_agent_change_propagates_deletion_to_peers() {
        let state = state_from(json!({ "mcpServers": { "s": 1 }, "keep": true }));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut peer = register_agent(&handles, "peer");
        // The editing container previously announced the full config.
        state.lock().await.snapshots.insert(
            "editor".to_string(),
            json!({ "mcpServers": { "s": 1 }, "keep": true }),
        );

        on_agent_change(&state, &handles, None, r#"{"keep":true}"#, "editor").await;

        assert_eq!(state.lock().await.canonical, json!({ "keep": true }));
        let DaemonMessage::SyncClaudeConfig { content } =
            peer.try_recv().expect("peer must be notified")
        else {
            panic!("expected SyncClaudeConfig");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).unwrap(),
            json!({ "keep": true }),
            "the deletion must propagate to peers"
        );
    }

    #[tokio::test]
    async fn on_agent_change_pushes_back_missing_keys() {
        // A blip-stale / reconnecting container re-announces content lacking a key
        // another source added; the daemon must push the full canonical back so it
        // converges. This is the reconnect-repair gate: canonical != incoming.
        let state = state_from(json!({
            "keep": true,
            "projects": { "/workspaces/z": { "k": 9 } }
        }));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");
        state
            .lock()
            .await
            .snapshots
            .insert("cella-a".to_string(), json!({ "keep": true }));

        on_agent_change(&state, &handles, None, r#"{"keep":true}"#, "cella-a").await;

        let DaemonMessage::SyncClaudeConfig { content } =
            agent.try_recv().expect("agent must receive a push-back")
        else {
            panic!("expected SyncClaudeConfig");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).unwrap()["projects"]["/workspaces/z"]
                ["k"],
            9,
            "push-back must carry the key the agent was missing"
        );
    }

    #[tokio::test]
    async fn on_agent_change_no_pushback_when_in_sync() {
        // No write-amplification: a steady-state edit that already matches
        // canonical gets no push-back echoed to its origin.
        let state = state_from(json!({ "a": 1 }));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");
        state
            .lock()
            .await
            .snapshots
            .insert("cella-a".to_string(), json!({ "a": 1 }));

        on_agent_change(&state, &handles, None, r#"{"a":1}"#, "cella-a").await;

        assert!(
            agent.try_recv().is_err(),
            "no push-back when the sender is already in sync"
        );
    }
}
