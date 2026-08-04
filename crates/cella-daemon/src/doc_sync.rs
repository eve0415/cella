//! Host-side merge hub for the forwarded Claude Code documents.
//!
//! One [`DocSyncState`] per [`SyncDoc`] (`~/.claude.json` and the two plugin
//! manifests). Each holds the canonical document, a snapshot of what the host
//! file last contained, and a content hash for loop suppression.
//!
//! Two inbound directions, deliberately asymmetric:
//! - **Agents send RFC 7386 merge patches** derived against their own persisted
//!   baseline, applied here directly. A key a container never touched is absent
//!   from its patch and therefore survives — which is what stops a container
//!   holding a create-time snapshot from reverting a peer's marketplace refresh.
//! - **The host sends whole documents** (it has no agent to derive a patch), so
//!   a host change is diffed against `host_snapshot` first. That snapshot is
//!   *not* interchangeable with `canonical`: canonical is the merged union and
//!   carries keys only some container ever had, so diffing against it would
//!   fabricate a `null` for every one of them and delete peer state.
//!
//! The daemon is the sole writer of the host files; agents never write them.
//! Canonical is broadcast to every opted-in agent, and each agent's own content
//! hash drops the echo. The sender is additionally always sent canonical, even
//! when its patch changed nothing — that is the only repair path for a container
//! that reconnects holding a stale copy it did not itself edit.
//!
//! Accepted limitation: concurrent edits to the same scalar resolve
//! last-writer-wins.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use cella_protocol::{DaemonMessage, SyncDoc};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::control_server::ContainerHandle;

/// Shared registry of connected containers (mirrors `ControlContext`).
type Handles = Arc<Mutex<HashMap<String, ContainerHandle>>>;

/// Canonical state for one synced document.
pub struct DocSyncState {
    /// Which document this hub owns, so reads and writes pick the right codec.
    doc: SyncDoc,
    /// Merged canonical document — host-shaped, and normalized for
    /// `InstalledPlugins`. A superset of the host file: it also carries keys
    /// that exist only in some container.
    canonical: serde_json::Value,
    /// What the host file last held, in canonical form. Distinct from
    /// `canonical` precisely because canonical is a superset — diffing a host
    /// edit against the union would fabricate a deletion for every
    /// container-only key.
    host_snapshot: serde_json::Value,
    /// SHA-256 of the raw bytes last written to / observed on the host file, so
    /// the daemon's own watcher event can be recognised and dropped.
    last_hash: String,
}

impl DocSyncState {
    /// Seed from the host file at startup. An absent or malformed file yields an
    /// empty object so merges still work.
    #[must_use]
    pub fn load(path: Option<&Path>, doc: SyncDoc) -> Self {
        let raw = path.and_then(|p| std::fs::read(p).ok());
        let canonical = raw
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|s| cella_env::claude_code::to_canonical(doc, s, None))
            .unwrap_or_else(|| serde_json::json!({}));
        let last_hash = raw
            .as_deref()
            .map(cella_filesync::sha256_hex)
            .unwrap_or_default();
        Self {
            doc,
            host_snapshot: canonical.clone(),
            canonical,
            last_hash,
        }
    }

    /// The canonical document in the host's on-disk form, for transport and for
    /// writing the host file. Routed through the codec so `installed_plugins`
    /// keeps its real entry-array schema on disk.
    fn on_disk_string(&self) -> String {
        cella_env::claude_code::to_local(self.doc, &self.canonical, None)
    }
}

/// Handle a host-side change to a synced document detected by the watcher.
///
/// Reads the file, drops the event if it is the daemon's own write, then diffs
/// the host content against `host_snapshot` to derive a merge patch (including
/// deletions) and applies it to the canonical document. Writes the merged result
/// back when the host file is missing container-only keys, and broadcasts the
/// new canonical to opted-in agents when it actually changed.
pub async fn on_host_change(
    state: &Arc<Mutex<DocSyncState>>,
    handles: &Handles,
    host_path: &Path,
    doc: SyncDoc,
) {
    let Ok(raw) = std::fs::read(host_path) else {
        debug!("doc sync: host file unreadable (mid-rename?); waiting for next event");
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

    let Some(incoming) = std::str::from_utf8(&raw)
        .ok()
        .and_then(|s| cella_env::claude_code::to_canonical(doc, s, None))
    else {
        warn!("doc sync: host {doc:?} is not valid JSON; skipping");
        return;
    };

    let (out, host_needs_update, canonical_changed) = {
        let mut st = state.lock().await;
        let patch = cella_env::claude_code::diff_merge_patch(&st.host_snapshot, &incoming);
        let merged = cella_env::claude_code::apply_merge_patch(&st.canonical, &patch);
        let canonical_changed = merged != st.canonical;
        st.canonical = merged;
        // Canonical may still hold container-only keys the host file lacks; write
        // them back so the host file converges to the union.
        let host_needs_update = st.canonical != incoming;
        st.host_snapshot = incoming;
        (st.on_disk_string(), host_needs_update, canonical_changed)
    };

    if host_needs_update {
        write_host_guarded(state, host_path, &out).await;
    }

    if canonical_changed {
        broadcast(handles, doc, &out).await;
    }
}

/// Write `out` to the host file, recording its hash as `last_hash` only on a
/// successful write. The recorded hash lets the self-triggered watcher event be
/// recognised as the daemon's own write and dropped; the watcher debounce is far
/// longer than a write+hash, so the hash is in place before the event arrives.
/// Recording it only on success means a failed write never leaves the daemon
/// believing stale content is on disk. Both write sites share this helper.
async fn write_host_guarded(state: &Arc<Mutex<DocSyncState>>, path: &Path, out: &str) {
    match cella_filesync::atomic_write(path, out.as_bytes(), 0o600) {
        Ok(()) => {
            let hash = cella_filesync::sha256_hex(out.as_bytes());
            let mut st = state.lock().await;
            let written = cella_env::claude_code::to_canonical(st.doc, out, None)
                .unwrap_or_else(|| serde_json::json!({}));
            st.last_hash = hash;
            // The host file now equals `out`; record it as the host snapshot so a
            // later host edit diffs against what's actually on disk.
            st.host_snapshot = written;
        }
        Err(e) => warn!("doc sync: failed to write host {}: {e}", path.display()),
    }
}

/// Apply an agent's merge patch to the canonical document.
///
/// The agent derives the patch against its own persisted baseline, so a key it
/// never touched is absent from the patch and survives untouched — which is what
/// stops a stale container from reverting a peer's change.
///
/// Canonical is always sent back to the sender, even when the patch changed
/// nothing. The daemon no longer holds a per-container document and so cannot
/// tell whether the sender is up to date; an unconditional reply is what repairs
/// a container that reconnects holding a copy it never edited. The agent's own
/// content hash makes a redundant reply a no-op.
pub async fn on_agent_change(
    state: &Arc<Mutex<DocSyncState>>,
    handles: &Handles,
    host_path: Option<&Path>,
    doc: SyncDoc,
    patch: &str,
    sender: &str,
) {
    let Ok(patch) = serde_json::from_str::<serde_json::Value>(patch) else {
        warn!("doc sync: container {sender} sent an invalid {doc:?} patch; skipping");
        return;
    };

    let (out, changed) = {
        let mut st = state.lock().await;
        let merged = cella_env::claude_code::apply_merge_patch(&st.canonical, &patch);
        let changed = merged != st.canonical;
        st.canonical = merged;
        (st.on_disk_string(), changed)
    };

    if !changed {
        // Nothing new for the peers, but the sender still gets canonical: its
        // patch may have been empty because it is *behind*, not in sync.
        send_to(handles, sender, doc, &out).await;
        return;
    }
    if let Some(path) = host_path {
        write_host_guarded(state, path, &out).await;
    }
    // Broadcast includes the sender — its own content hash drops the echo, so a
    // separate reply would be a duplicate.
    broadcast(handles, doc, &out).await;
}

/// Send one canonical document to a connected agent.
async fn push(tx: &tokio::sync::mpsc::Sender<DaemonMessage>, doc: SyncDoc, content: &str) {
    let _ = tx
        .send(DaemonMessage::SyncConfigDoc {
            doc,
            content: content.to_string(),
        })
        .await;
}

/// Send `content` as a `SyncConfigDoc` to every opted-in connected agent,
/// including the origin of an inbound change — its own content hash drops the
/// echo, so excluding it would only cost a branch.
async fn broadcast(handles: &Handles, doc: SyncDoc, content: &str) {
    // Clone the senders under the lock, then send after releasing it — never
    // hold the registry mutex across an await.
    let senders: Vec<tokio::sync::mpsc::Sender<DaemonMessage>> = {
        let registry = handles.lock().await;
        registry
            .iter()
            .filter(|(_, h)| h.claude_config_sync)
            .filter_map(|(_, h)| h.agent_tx.clone())
            .collect()
    };

    for tx in senders {
        push(&tx, doc, content).await;
    }
}

/// Send `content` as a `SyncConfigDoc` to a single opted-in agent by name.
/// Used to converge the sender of a patch (reconnect/catch-up repair).
async fn send_to(handles: &Handles, name: &str, doc: SyncDoc, content: &str) {
    let tx = {
        let registry = handles.lock().await;
        registry
            .get(name)
            .filter(|h| h.claude_config_sync)
            .and_then(|h| h.agent_tx.clone())
    };
    if let Some(tx) = tx {
        push(&tx, doc, content).await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn state_from(json: serde_json::Value, doc: SyncDoc) -> Arc<Mutex<DocSyncState>> {
        let bytes = serde_json::to_vec(&json).expect("serializes");
        Arc::new(Mutex::new(DocSyncState {
            doc,
            host_snapshot: json.clone(),
            canonical: json,
            last_hash: cella_filesync::sha256_hex(&bytes),
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
        let st = DocSyncState::load(None, SyncDoc::ClaudeJson);
        assert_eq!(st.canonical, json!({}));
        assert!(st.last_hash.is_empty());
    }

    /// The incident this change exists for: container A holds a create-time
    /// snapshot predating a marketplace refresh made in container B. When A's
    /// Claude Code touches one entry, A must not revert the others. A
    /// whole-document push does exactly that; a patch cannot.
    #[tokio::test]
    async fn stale_container_patch_does_not_revert_peer_refresh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let host = tmp.path().join("known_marketplaces.json");
        std::fs::write(
            &host,
            r#"{
              "official": {"lastUpdated":"2026-08-04T07:08:16Z"},
              "codex":    {"lastUpdated":"2026-08-04T07:08:15Z"},
              "skills":   {"lastUpdated":"2026-08-04T07:08:17Z"}
            }"#,
        )
        .expect("seed host");

        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // Container A refreshed only `codex`; its patch mentions nothing else.
        let patch = r#"{"codex":{"lastUpdated":"2026-08-04T07:11:16Z"}}"#;
        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            patch,
            "a",
        )
        .await;

        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host).expect("read host")).expect("valid json");
        assert_eq!(
            after["codex"]["lastUpdated"],
            json!("2026-08-04T07:11:16Z"),
            "A's own edit applied"
        );
        assert_eq!(
            after["official"]["lastUpdated"],
            json!("2026-08-04T07:08:16Z"),
            "peer refresh survives"
        );
        assert_eq!(
            after["skills"]["lastUpdated"],
            json!("2026-08-04T07:08:17Z"),
            "peer refresh survives"
        );
    }

    #[tokio::test]
    async fn on_agent_change_merges_and_preserves_host_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        // Canonical/host start with a host-namespaced project.
        let initial = json!({
            "projects": { "/Users/eve/p": { "k": 1 } }
        });
        std::fs::write(
            &host,
            serde_json::to_vec_pretty(&initial).expect("serializes"),
        )
        .expect("seed host");
        let state = state_from(initial, SyncDoc::ClaudeJson);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // Container patches in its own (disjoint) project namespace.
        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::ClaudeJson,
            r#"{"projects":{"/workspaces/p":{"k":2}}}"#,
            "cella-test",
        )
        .await;

        // Host file now contains BOTH project namespaces (deep-merge union).
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host).expect("read host")).expect("valid json");
        assert_eq!(written["projects"]["/Users/eve/p"]["k"], 1);
        assert_eq!(written["projects"]["/workspaces/p"]["k"], 2);
    }

    #[tokio::test]
    async fn write_host_guarded_keeps_hash_when_write_fails() {
        // On a write failure the hash must NOT advance — otherwise the daemon
        // believes the (never-written) content is the host's on-disk state, and
        // a restart would re-seed from a stale file.
        let state = state_from(json!({ "a": 1 }), SyncDoc::ClaudeJson);
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
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        let state = state_from(json!({ "a": 1 }), SyncDoc::ClaudeJson);
        write_host_guarded(&state, &host, r#"{"a":2}"#).await;
        assert_eq!(
            state.lock().await.last_hash,
            cella_filesync::sha256_hex(br#"{"a":2}"#)
        );
        assert_eq!(
            std::fs::read_to_string(&host).expect("read host"),
            r#"{"a":2}"#
        );
    }

    #[tokio::test]
    async fn on_agent_change_ignores_invalid_patch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        std::fs::write(&host, b"{}").expect("seed host");
        let state = state_from(json!({}), SyncDoc::ClaudeJson);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::ClaudeJson,
            "not json",
            "cella-test",
        )
        .await;
        // Host file untouched, no panic.
        assert_eq!(std::fs::read(&host).expect("read host"), b"{}");
    }

    #[tokio::test]
    async fn on_host_change_propagates_deletion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        let initial = json!({ "mcpServers": { "s": 1 }, "keep": true });
        std::fs::write(&host, serde_json::to_vec(&initial).expect("serializes"))
            .expect("seed host");
        let state = state_from(initial, SyncDoc::ClaudeJson);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // User removes mcpServers on the host.
        std::fs::write(
            &host,
            serde_json::to_vec(&json!({ "keep": true })).expect("serializes"),
        )
        .expect("host edit");
        on_host_change(&state, &handles, &host, SyncDoc::ClaudeJson).await;

        assert_eq!(
            state.lock().await.canonical,
            json!({ "keep": true }),
            "a host-side deletion must drop the key from canonical"
        );
    }

    /// Canonical is a superset of the host file, so `host_snapshot` cannot be
    /// collapsed into `canonical`: a host edit diffed against the union would
    /// fabricate a `null` for every key only a container ever had.
    #[tokio::test]
    async fn on_host_change_preserves_container_only_keys() {
        // A host edit must not delete keys the host never had (container-only).
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        let host_view = json!({ "keep": true });
        std::fs::write(&host, serde_json::to_vec(&host_view).expect("serializes"))
            .expect("seed host");
        let state = state_from(
            json!({
                "keep": true,
                "projects": { "/workspaces/p": { "k": 2 } }
            }),
            SyncDoc::ClaudeJson,
        );
        // host_snapshot reflects what the host last had (no container key).
        state.lock().await.host_snapshot = host_view;
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // Host adds a key; the container-only project must survive.
        std::fs::write(
            &host,
            serde_json::to_vec(&json!({ "keep": true, "theme": "dark" })).expect("serializes"),
        )
        .expect("host edit");
        on_host_change(&state, &handles, &host, SyncDoc::ClaudeJson).await;

        let canon = state.lock().await.canonical.clone();
        assert_eq!(canon["projects"]["/workspaces/p"]["k"], 2);
        assert_eq!(canon["theme"], "dark");
    }

    #[tokio::test]
    async fn on_agent_change_propagates_deletion_to_peers() {
        let state = state_from(
            json!({ "mcpServers": { "s": 1 }, "keep": true }),
            SyncDoc::ClaudeJson,
        );
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut peer = register_agent(&handles, "peer");

        // The editing container's patch expresses the removal explicitly.
        on_agent_change(
            &state,
            &handles,
            None,
            SyncDoc::ClaudeJson,
            r#"{"mcpServers":null}"#,
            "editor",
        )
        .await;

        assert_eq!(state.lock().await.canonical, json!({ "keep": true }));
        let DaemonMessage::SyncConfigDoc { content, .. } =
            peer.try_recv().expect("peer must be notified")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json"),
            json!({ "keep": true }),
            "the deletion must propagate to peers"
        );
    }

    /// A container that reconnects having changed nothing sends an empty patch.
    /// It must still get canonical back — that is the only path by which it
    /// learns about changes it missed while disconnected.
    #[tokio::test]
    async fn empty_patch_still_replies_with_canonical() {
        let state = state_from(
            json!({
                "keep": true,
                "projects": { "/workspaces/z": { "k": 9 } }
            }),
            SyncDoc::ClaudeJson,
        );
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        on_agent_change(&state, &handles, None, SyncDoc::ClaudeJson, "{}", "cella-a").await;

        let DaemonMessage::SyncConfigDoc { content, .. } =
            agent.try_recv().expect("agent must receive canonical")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["projects"]["/workspaces/z"]
                ["k"],
            9,
            "the reply must carry the key the agent was missing"
        );
    }

    /// Wiring guard for the two new hubs: a host-side plugin manifest edit must
    /// reach opted-in agents tagged with the right document, the direction that
    /// did not exist at all before (the old sync was container -> host only).
    #[tokio::test]
    async fn host_edit_to_a_plugin_manifest_broadcasts_to_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        std::fs::write(&host, r#"{"official":{"lastUpdated":"1"}}"#).expect("seed host");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        std::fs::write(&host, r#"{"official":{"lastUpdated":"2"}}"#).expect("host edit");
        on_host_change(&state, &handles, &host, SyncDoc::KnownMarketplaces).await;

        let DaemonMessage::SyncConfigDoc { doc, content } =
            agent.try_recv().expect("agent must be notified")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(doc, SyncDoc::KnownMarketplaces);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["official"]["lastUpdated"],
            json!("2")
        );
    }

    /// The host file keeps its real entry-array schema; the normalized
    /// context-keyed form is in-memory and on-the-wire only.
    #[tokio::test]
    async fn installed_plugins_host_file_keeps_entry_arrays() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("installed_plugins.json");
        std::fs::write(
            &host,
            r#"{"version":2,"plugins":{"p@m":[{"scope":"user","version":"1.0"}]}}"#,
        )
        .expect("seed host");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::InstalledPlugins,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::InstalledPlugins,
            r#"{"plugins":{"p@m":{"user":{"version":"2.0"}}}}"#,
            "a",
        )
        .await;

        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host).expect("read host")).expect("valid json");
        let entries = after["plugins"]["p@m"]
            .as_array()
            .expect("on-disk form is an array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["version"], json!("2.0"));
        assert_eq!(entries[0]["scope"], json!("user"));
    }
}
