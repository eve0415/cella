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
//!   That baseline lives in the container and outlives the daemon, so a patch is
//!   a delta against state a restarted hub may not have: a patch never seeds.
//!   An agent also announces its whole document at every (re)connect, and that
//!   is what seeds a hub whose host file was absent or malformed at startup —
//!   see [`on_agent_snapshot`].
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
    /// Set when a host write failed after canonical had already moved. Without
    /// it the host stays stale indefinitely: `last_hash` still names the
    /// unchanged file, so nothing schedules a retry.
    host_dirty: bool,
    /// This host's `~/.claude`, used to repair a previous writer's home out of
    /// the host document. `None` when it cannot be resolved, which makes the
    /// repair a no-op.
    host_claude: Option<String>,
    /// Monotonic revision of `canonical`, stamped on every push so an agent can
    /// drop a stale one that overtook a newer push in flight.
    rev: u64,
    /// Whether this hub loaded its state from the host file at startup.
    ///
    /// Fixed for the life of the daemon, and the gate on
    /// [`on_agent_snapshot`]. An agent's baseline lives in its container and so
    /// outlives the daemon: a hub that came up without the host file knows less
    /// than every connected container's baseline claims, and the deltas it
    /// receives are fragments it must not push. Such a hub takes whole
    /// documents from containers instead; one that did load the host file does
    /// not, so a reconnecting container can never re-assert its whole document
    /// over a peer's newer state.
    started_from_host: bool,
    /// Whether any complete source has ever supplied content.
    ///
    /// An absent or malformed host file loads canonical as `{}`, which is
    /// indistinguishable from a genuinely empty document. Pushing that to a
    /// container whose own copy is intact would destroy the only good copy —
    /// and the container cannot object, since its reannounce is an empty patch
    /// (its file matches its baseline). Until something seeds it, the hub
    /// neither pushes nor writes the host file.
    ///
    /// Only a source known to be whole sets this: the host file, or a
    /// container's [`AgentMessage::ConfigDocSnapshot`](cella_protocol::AgentMessage).
    /// A patch cannot, however much content it carries — it is a delta against
    /// a baseline the daemon does not hold, so canonical after applying one is
    /// a fragment, and an agent applies a push wholesale.
    seeded: bool,
}

impl DocSyncState {
    /// Seed from the host file at startup. An absent or malformed file yields an
    /// empty object so merges still work.
    #[must_use]
    pub fn load(path: Option<&Path>, doc: SyncDoc) -> Self {
        let host_claude =
            cella_env::claude_code::host_claude_dir().map(|d| d.to_string_lossy().into_owned());
        let raw = path.and_then(|p| std::fs::read(p).ok());
        let parsed = raw
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|s| host_canonical(doc, s, host_claude.as_deref()));
        let started_from_host = parsed.is_some();
        let canonical = parsed.unwrap_or_else(|| serde_json::json!({}));
        let last_hash = raw
            .as_deref()
            .map(cella_filesync::sha256_hex)
            .unwrap_or_default();
        Self {
            doc,
            host_snapshot: canonical.clone(),
            canonical,
            last_hash,
            host_dirty: false,
            host_claude,
            started_from_host,
            seeded: started_from_host,
            rev: 0,
        }
    }

    /// The canonical document in the host's on-disk form, for writing the host
    /// file. Routed through the codec so `installed_plugins` keeps its real
    /// entry-array schema on disk.
    fn on_disk_string(&self) -> String {
        cella_env::claude_code::to_local(self.doc, &self.canonical, None)
    }

    /// The canonical document as sent to agents — canonical form verbatim, NOT
    /// the on-disk form.
    ///
    /// These differ for `InstalledPlugins`: on disk the entries are arrays,
    /// canonically they are keyed by install context. An agent that persisted
    /// the array shape as its baseline would diff its next normalized read
    /// against it as a whole-value replacement and send back a full-entry patch
    /// that clobbers peers — the exact failure this hub exists to remove.
    fn wire_string(&self) -> String {
        serde_json::to_string(&self.canonical).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Parse a host document into canonical form, repairing any previous writer's
/// `.claude` home to this host's.
///
/// Canonical is defined as host-shaped, and agents translate only the exact
/// current host prefix. A manifest still carrying `/home/node/.claude/…` from a
/// container that ran as another user would therefore survive a daemon push
/// untranslated and overwrite the correctly repaired create-time seed, leaving
/// the plugin unresolvable in the container.
fn host_canonical(doc: SyncDoc, raw: &str, host_claude: Option<&str>) -> Option<serde_json::Value> {
    let canonical = cella_env::claude_code::to_canonical(doc, raw, None)?;
    Some(match host_claude {
        Some(home) => cella_env::claude_code::repair_claude_home(&canonical, home),
        None => canonical,
    })
}

/// Apply `mutate` to the canonical document and, in the *same* critical section,
/// write the host file and record its hash and snapshot.
///
/// One transaction per document. With the mutation and the write in separate
/// sections, two agents patching concurrently can serialize their canonical
/// updates one way and their host writes the other: the older output lands last,
/// leaving the host behind canonical while `last_hash` names the older bytes —
/// which then suppresses the watcher event that would have repaired it, so the
/// newer change is lost on the next daemon restart. Nothing here awaits, so
/// holding the guard costs one file write.
///
/// Returns the canonical document for the wire, and whether it changed.
async fn transact(
    state: &Arc<Mutex<DocSyncState>>,
    host_path: Option<&Path>,
    mutate: impl FnOnce(&mut DocSyncState) -> bool,
) -> Option<(String, u64, bool)> {
    let mut st = state.lock().await;
    let changed = mutate(&mut st);
    if changed {
        st.rev += 1;
    }
    // Gated on `seeded` for the same reason the push is: before a complete
    // source has been seen, canonical is whatever fragments happen to have
    // arrived, and writing that to the host file would make the fragment the
    // authoritative document the *next* daemon loads and pushes to everyone.
    //
    // `host_dirty` carries a previously failed write: canonical had already
    // moved, so without it a transient error would leave the host stale forever
    // — `last_hash` still names the unchanged file, so `ingest_host` returns
    // early and nothing ever schedules the rewrite.
    if let Some(path) = host_path
        && st.seeded
        && (st.canonical != st.host_snapshot || st.host_dirty)
    {
        write_host(&mut st, path);
    }
    // Nothing may be pushed before a valid source has been seen: canonical is
    // `{}` only because the host file was missing or malformed, and a container
    // holding the one good copy would have it overwritten.
    st.seeded.then(|| (st.wire_string(), st.rev, changed))
}

/// Write the canonical document to the host file, recording its hash only on a
/// successful write.
///
/// The recorded hash lets the self-triggered watcher event be recognised as the
/// daemon's own write and dropped. Recording it only on success means a failed
/// write never leaves the daemon believing stale content is on disk; the failure
/// sets `host_dirty` so the next transaction retries.
fn write_host(st: &mut DocSyncState, path: &Path) {
    let out = st.on_disk_string();
    match cella_filesync::atomic_write(path, out.as_bytes(), 0o600) {
        Ok(()) => {
            st.last_hash = cella_filesync::sha256_hex(out.as_bytes());
            // The host file now equals `out`; record it as the host snapshot so a
            // later host edit diffs against what's actually on disk.
            st.host_snapshot = host_canonical(st.doc, &out, st.host_claude.as_deref())
                .unwrap_or_else(|| serde_json::json!({}));
            st.host_dirty = false;
        }
        Err(e) => {
            warn!("doc sync: failed to write host {}: {e}", path.display());
            st.host_dirty = true;
        }
    }
}

/// Handle a host-side change to a synced document detected by the watcher.
///
/// Folds the host file into the canonical document and broadcasts the result to
/// opted-in agents when it actually changed. The host file is rewritten in the
/// same transaction when canonical still holds container-only keys it lacks.
pub async fn on_host_change(
    state: &Arc<Mutex<DocSyncState>>,
    handles: &Handles,
    host_path: &Path,
    doc: SyncDoc,
) {
    let Some((wire, rev, changed)) =
        transact(state, Some(host_path), |st| ingest_host(st, host_path, doc)).await
    else {
        return;
    };
    if changed {
        broadcast(handles, doc, rev, &wire).await;
    }
}

/// Fold whatever the host file currently holds into the canonical document,
/// returning whether canonical changed.
///
/// Called under the transaction guard, and also by [`on_agent_change`] before it
/// applies a patch — so an agent patch is never written on top of a host edit
/// the watcher has not debounced yet. That write would advance `last_hash`, and
/// the coalesced watcher event would then be discarded as the daemon's own,
/// silently dropping the host edit.
fn ingest_host(st: &mut DocSyncState, host_path: &Path, doc: SyncDoc) -> bool {
    let Ok(raw) = std::fs::read(host_path) else {
        debug!("doc sync: host file unreadable (mid-rename?); waiting for next event");
        return false;
    };

    let incoming_hash = cella_filesync::sha256_hex(&raw);
    if incoming_hash == st.last_hash {
        return false; // our own write, or already processed
    }
    st.last_hash = incoming_hash;

    let Some(incoming) = std::str::from_utf8(&raw)
        .ok()
        .and_then(|s| host_canonical(doc, s, st.host_claude.as_deref()))
    else {
        // `last_hash` still advanced, so the same invalid bytes are not
        // re-parsed on every subsequent event.
        warn!("doc sync: host {doc:?} is not valid JSON; skipping");
        return false;
    };

    // A parseable host file is a valid source, so the hub may speak from here on.
    st.seeded = true;
    let patch = cella_env::claude_code::diff_documents(doc, &st.host_snapshot, &incoming);
    let merged = cella_env::claude_code::apply_merge_patch(&st.canonical, &patch);
    let changed = merged != st.canonical;
    st.canonical = merged;
    st.host_snapshot = incoming;
    changed
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

    let Some((wire, rev, changed)) = transact(state, host_path, |st| {
        // Fold in any host edit the watcher has not debounced yet, so this patch
        // lands on the host's real current state rather than a stale canonical.
        let host_changed = host_path.is_some_and(|path| ingest_host(st, path, doc));
        if !st.seeded {
            // A delta means nothing to a hub with no state to apply it to: it
            // was derived against a baseline that lives in the container, which
            // this daemon has never seen. Keeping the result would be worse than
            // dropping it — whatever seeds the hub next merges on top and then
            // publishes the fragment as if it were part of a document, and a
            // push is applied wholesale. Nothing is lost: no reply goes out, so
            // the sender's baseline does not advance and the edit comes back
            // with its next announce.
            debug!("doc sync: {doc:?} patch from {sender} dropped; hub has no source yet");
            return host_changed;
        }
        let merged = cella_env::claude_code::apply_merge_patch(&st.canonical, &patch);
        let patch_changed = merged != st.canonical;
        st.canonical = merged;
        // Deliberately never seeds: `patch` is a delta against a baseline this
        // daemon does not hold, so a non-empty one says nothing about how much
        // of the container's document canonical now has. `ConfigDocSnapshot` is
        // the message that seeds an empty hub.
        host_changed || patch_changed
    })
    .await
    else {
        return;
    };

    reply(handles, doc, sender, rev, &wire, changed).await;
}

/// Seed a hub that has no state of its own from one container's whole document.
///
/// Sent by every agent on every (re)connect, and accepted only while
/// `started_from_host` is false — the one case where the daemon's own memory of
/// these documents did not survive its restart but the containers' baselines
/// did. Merged, never replaced: two containers that each hold half the
/// marketplaces converge on the union, and no key is ever removed.
///
/// A hub that did load the host file drops the snapshot and answers with
/// canonical, exactly as it answers an empty patch. Accepting it there would
/// undo what patches exist for, letting a container that reconnects holding a
/// stale copy re-assert all of it over a peer's newer state.
///
/// Note what "merged" costs on a hub that never read the host file: the
/// container's value wins for a key both hold, so a container reconnecting with
/// a stale copy can put an older value back — including over content the host
/// file supplied later in that daemon's life, since a hub that started without
/// it keeps accepting announcements. That is the deliberate direction of the
/// trade. Refusing announcements once anything seeded the hub would instead
/// leave every container that has not yet announced facing a canonical that
/// cannot contain its keys — and it would be overwritten by the first push.
pub async fn on_agent_snapshot(
    state: &Arc<Mutex<DocSyncState>>,
    handles: &Handles,
    host_path: Option<&Path>,
    doc: SyncDoc,
    content: &str,
    sender: &str,
) {
    let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(content) else {
        warn!("doc sync: container {sender} sent an invalid {doc:?} snapshot; skipping");
        return;
    };
    if !snapshot.is_object() {
        // Under RFC 7386 a non-object patch *replaces* the target rather than
        // merging into it, which would discard every peer's keys.
        warn!("doc sync: container {sender} sent a non-object {doc:?} snapshot; skipping");
        return;
    }

    let Some((wire, rev, changed)) = transact(state, host_path, |st| {
        // Same reason as in `on_agent_change`: fold in any host edit the watcher
        // has not debounced yet before this lands on top of it.
        let host_changed = host_path.is_some_and(|path| ingest_host(st, path, doc));
        if st.started_from_host {
            return host_changed;
        }
        // Repair a previous writer's `.claude` home for the same reason
        // `host_canonical` does: an agent translates only its own prefix, so an
        // entry inherited from a container that ran as another user would reach
        // the peers unresolvable.
        let snapshot = match st.host_claude.as_deref() {
            Some(home) => cella_env::claude_code::repair_claude_home(&snapshot, home),
            None => snapshot,
        };
        let merged = merge_document(&st.canonical, &snapshot);
        let snapshot_changed = merged != st.canonical;
        st.canonical = merged;
        // A whole document is a complete source. The criterion is the *result*,
        // not that the snapshot was non-empty: a container whose own document is
        // empty leaves nothing behind worth pushing.
        st.seeded |= st.canonical.as_object().is_some_and(|o| !o.is_empty());
        host_changed || snapshot_changed
    })
    .await
    else {
        return;
    };

    reply(handles, doc, sender, rev, &wire, changed).await;
}

/// Merge a container's whole document into canonical.
///
/// Deliberately not `apply_merge_patch`: RFC 7386 reads a `null` as a tombstone,
/// which is right for a patch and wrong for a document — a container whose file
/// happens to hold `"k": null` would delete `k` from the host and every peer.
/// Objects merge key by key; anything else takes the incoming value.
fn merge_document(base: &serde_json::Value, incoming: &serde_json::Value) -> serde_json::Value {
    let (Some(base_obj), Some(incoming_obj)) = (base.as_object(), incoming.as_object()) else {
        return incoming.clone();
    };
    let mut out = base_obj.clone();
    for (key, value) in incoming_obj {
        let merged = out
            .get(key)
            .map_or_else(|| value.clone(), |held| merge_document(held, value));
        out.insert(key.clone(), merged);
    }
    serde_json::Value::Object(out)
}

/// Deliver the outcome of one inbound agent message.
///
/// A change goes to every opted-in container, the sender included — its own
/// content hash drops the echo, and the reply is what advances its baseline.
/// When nothing changed the sender alone is answered: its message may have
/// carried nothing because it is *behind*, not because it is in sync.
async fn reply(handles: &Handles, doc: SyncDoc, sender: &str, rev: u64, wire: &str, changed: bool) {
    if changed {
        broadcast(handles, doc, rev, wire).await;
    } else {
        send_to(handles, sender, doc, rev, wire).await;
    }
}

/// Send one canonical document to a connected agent.
async fn push(
    tx: &tokio::sync::mpsc::Sender<DaemonMessage>,
    doc: SyncDoc,
    rev: u64,
    content: &str,
) {
    let _ = tx
        .send(DaemonMessage::SyncConfigDoc {
            doc,
            rev,
            content: content.to_string(),
        })
        .await;
}

/// Send `content` as a `SyncConfigDoc` to every opted-in connected agent,
/// including the origin of an inbound change — its own content hash drops the
/// echo, so excluding it would only cost a branch.
async fn broadcast(handles: &Handles, doc: SyncDoc, rev: u64, content: &str) {
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
        push(&tx, doc, rev, content).await;
    }
}

/// Send `content` as a `SyncConfigDoc` to a single opted-in agent by name.
/// Used to converge the sender of a patch (reconnect/catch-up repair).
async fn send_to(handles: &Handles, name: &str, doc: SyncDoc, rev: u64, content: &str) {
    let tx = {
        let registry = handles.lock().await;
        registry
            .get(name)
            .filter(|h| h.claude_config_sync)
            .and_then(|h| h.agent_tx.clone())
    };
    if let Some(tx) = tx {
        push(&tx, doc, rev, content).await;
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
            host_dirty: false,
            host_claude: Some("/Users/alice/.claude".to_string()),
            started_from_host: true,
            seeded: true,
            rev: 0,
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
    async fn write_host_keeps_hash_and_marks_dirty_when_write_fails() {
        // On a write failure the hash must NOT advance — otherwise the daemon
        // believes the (never-written) content is the host's on-disk state, and
        // a restart would re-seed from a stale file. `host_dirty` is what makes
        // the next transaction retry instead of leaving the host stale forever.
        let state = state_from(json!({ "a": 1 }), SyncDoc::ClaudeJson);
        let before = state.lock().await.last_hash.clone();
        // A path whose parent directory does not exist makes atomic_write fail.
        let bad = Path::new("/nonexistent-cella-xyz/.claude.json");
        write_host(&mut *state.lock().await, bad);
        let (last_hash, dirty) = {
            let st = state.lock().await;
            (st.last_hash.clone(), st.host_dirty)
        };
        assert_eq!(
            last_hash, before,
            "a failed host write must not advance last_hash"
        );
        assert!(dirty, "a failed write must be marked for retry");
    }

    #[tokio::test]
    async fn write_host_advances_hash_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        let state = state_from(json!({ "a": 1 }), SyncDoc::ClaudeJson);
        let expected = state.lock().await.on_disk_string();
        write_host(&mut *state.lock().await, &host);
        let (last_hash, dirty) = {
            let st = state.lock().await;
            (st.last_hash.clone(), st.host_dirty)
        };
        assert_eq!(last_hash, cella_filesync::sha256_hex(expected.as_bytes()));
        assert!(!dirty);
        assert_eq!(std::fs::read_to_string(&host).expect("read host"), expected);
    }

    /// A transient write failure must not strand the host file: canonical has
    /// already moved, and `last_hash` still names the unchanged file, so nothing
    /// else would ever schedule the rewrite.
    #[tokio::test]
    async fn a_failed_host_write_is_retried_on_the_next_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        let state = state_from(json!({}), SyncDoc::KnownMarketplaces);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        // First patch: the parent directory does not exist yet, so the write fails.
        let missing = dir.path().join("gone").join("known_marketplaces.json");
        on_agent_change(
            &state,
            &handles,
            Some(&missing),
            SyncDoc::KnownMarketplaces,
            r#"{"a":{"lastUpdated":"1"}}"#,
            "cella-a",
        )
        .await;
        assert!(state.lock().await.host_dirty, "precondition: write failed");

        // A later transaction against a writable path must flush it.
        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            "{}",
            "cella-a",
        )
        .await;
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host).expect("read host")).expect("valid json");
        assert_eq!(written["a"]["lastUpdated"], json!("1"));
        assert!(!state.lock().await.host_dirty);
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

        let DaemonMessage::SyncConfigDoc { doc, content, .. } =
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

    /// The wire must carry *canonical* (context-keyed) form, not the on-disk
    /// entry arrays. An agent that persists an array-shaped baseline would diff
    /// its next normalized read against it as a whole-value replacement, and
    /// send back a full-entry patch that clobbers peers — reintroducing exactly
    /// the bug this hub exists to remove.
    #[tokio::test]
    async fn broadcast_carries_normalized_installed_plugins() {
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
        let mut agent = register_agent(&handles, "cella-a");

        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::InstalledPlugins,
            r#"{"plugins":{"p@m":{"user":{"version":"2.0"}}}}"#,
            "cella-a",
        )
        .await;

        let DaemonMessage::SyncConfigDoc { content, .. } =
            agent.try_recv().expect("agent must be notified")
        else {
            panic!("expected SyncConfigDoc");
        };
        let sent: serde_json::Value = serde_json::from_str(&content).expect("valid json");
        assert!(
            sent["plugins"]["p@m"].is_object(),
            "the wire form must stay context-keyed, got: {}",
            sent["plugins"]["p@m"]
        );
        assert_eq!(sent["plugins"]["p@m"]["user"]["version"], json!("2.0"));
    }

    /// A host edit swallowed by `ingest_host` during a no-op agent patch must
    /// still reach the peers. `ingest_host` advances `last_hash`, so the watcher
    /// event that would have delivered it is suppressed — dropping the result
    /// here loses the edit entirely.
    #[tokio::test]
    async fn host_edit_consumed_by_a_noop_patch_still_broadcasts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let host = tmp.path().join("known_marketplaces.json");
        std::fs::write(&host, r#"{"official":{"lastUpdated":"1"}}"#).expect("seed host");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut peer = register_agent(&handles, "peer");

        // The host is edited, but an agent's empty re-announce arrives first.
        std::fs::write(&host, r#"{"official":{"lastUpdated":"2"}}"#).expect("host edit");
        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            "{}",
            "cella-a",
        )
        .await;

        let DaemonMessage::SyncConfigDoc { content, .. } =
            peer.try_recv().expect("peer must receive the host edit")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["official"]["lastUpdated"],
            json!("2"),
            "the host edit must reach peers even when the agent patch is a no-op"
        );
    }

    /// Canonical is defined as host-shaped. A manifest still carrying another
    /// writer's home must be repaired on the way in, or a push would send that
    /// path to every container — where nothing translates it.
    #[tokio::test]
    async fn a_foreign_writers_home_is_repaired_into_canonical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        std::fs::write(
            &host,
            r#"{"m":{"installLocation":"/home/node/.claude/plugins/marketplaces/m"}}"#,
        )
        .expect("seed host");
        let state = state_from(json!({}), SyncDoc::KnownMarketplaces);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        on_host_change(&state, &handles, &host, SyncDoc::KnownMarketplaces).await;

        assert_eq!(
            state.lock().await.canonical["m"]["installLocation"],
            json!("/Users/alice/.claude/plugins/marketplaces/m"),
            "canonical must be host-shaped"
        );
        let DaemonMessage::SyncConfigDoc { content, .. } =
            agent.try_recv().expect("agent must be notified")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["m"]["installLocation"],
            json!("/Users/alice/.claude/plugins/marketplaces/m")
        );
    }

    /// An unseeded hub — host file absent or malformed at startup — must stay
    /// silent. Its canonical is `{}` only because it has no source, and a
    /// container holding the one good copy cannot object: its reannounce is an
    /// empty patch, because its file matches its own baseline.
    #[tokio::test]
    async fn an_unseeded_hub_pushes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        // No host file at all.
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        assert!(!state.lock().await.seeded, "precondition");
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            "{}",
            "cella-a",
        )
        .await;

        assert!(
            agent.try_recv().is_err(),
            "an unseeded hub must not push an empty canonical over a good copy"
        );
    }

    /// An agent's patch is a *delta* against that container's own persisted
    /// baseline, not its document — and that baseline outlives the daemon, so a
    /// daemon that starts with no host file receives, from a container holding
    /// `{a, b}` that touched only `b`, just `{"b": …}`. Canonical becomes that
    /// fragment. Seeding on it would push the fragment back, and an agent
    /// applies a push wholesale: `a` would be deleted in the sender and in every
    /// peer. A delta is never a complete source, and the fragment must not reach
    /// the host file either — a later daemon would load it as authoritative.
    #[tokio::test]
    async fn a_delta_patch_does_not_seed_an_empty_hub() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        // No host file at all.
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"b":{"lastUpdated":"2"}}"#,
            "cella-a",
        )
        .await;

        assert!(
            !state.lock().await.seeded,
            "a delta against a container-side baseline is not a complete source"
        );
        assert!(
            agent.try_recv().is_err(),
            "a fragment must not be pushed: the agent applies it wholesale"
        );
        assert!(
            !host.exists(),
            "and must not be persisted as the host document"
        );
        assert_eq!(
            state.lock().await.canonical,
            json!({}),
            "the fragment must not be kept either: whatever seeds the hub next \
             would merge on top and publish it as part of a document"
        );
    }

    /// The same delta, followed by the announcement that seeds the hub. What
    /// reaches the peers must be the announced document — a fragment left over
    /// from the unseeded window would be published as if it belonged to it.
    #[tokio::test]
    async fn a_pre_seed_delta_is_not_published_by_a_later_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        // No host file at all.
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut b = register_agent(&handles, "cella-b");

        // A edits one key of a document the hub has never seen.
        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"from-a":{"lastUpdated":"2"}}"#,
            "cella-a",
        )
        .await;
        // B then announces what it holds, which seeds the hub.
        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"from-b":{"lastUpdated":"1"}}"#,
            "cella-b",
        )
        .await;

        let DaemonMessage::SyncConfigDoc { content, .. } =
            b.try_recv().expect("the seed is pushed")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json"),
            json!({"from-b":{"lastUpdated":"1"}}),
            "only the announced document may be published"
        );
    }

    /// A snapshot is a document, not a patch: a `null` in it is a value the
    /// container happens to hold, not a tombstone for a peer's key.
    #[tokio::test]
    async fn a_null_in_a_snapshot_deletes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join(".claude.json");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::ClaudeJson,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));

        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::ClaudeJson,
            r#"{"kept":1,"nulled":2}"#,
            "cella-a",
        )
        .await;
        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::ClaudeJson,
            r#"{"nulled":null}"#,
            "cella-b",
        )
        .await;

        assert_eq!(
            state.lock().await.canonical,
            json!({"kept":1,"nulled":null}),
            "the null replaced a value; it must not have removed the key"
        );
    }

    #[test]
    fn merge_document_unions_objects_and_takes_incoming_leaves() {
        let merged = merge_document(
            &json!({"a":{"x":1,"y":2},"b":1}),
            &json!({"a":{"y":3,"z":4},"c":5}),
        );
        assert_eq!(merged, json!({"a":{"x":1,"y":3,"z":4},"b":1,"c":5}));
    }

    /// A host file that was malformed at startup seeds the hub as soon as it
    /// parses, and normal push behaviour resumes from there.
    #[tokio::test]
    async fn a_readable_host_file_seeds_an_empty_hub() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        std::fs::write(&host, "{").expect("malformed host file");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        assert!(!state.lock().await.seeded, "precondition");
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        // The host file becomes valid; ingest seeds the hub.
        std::fs::write(&host, r#"{"a":{"lastUpdated":"1"}}"#).expect("host edit");
        on_host_change(&state, &handles, &host, SyncDoc::KnownMarketplaces).await;

        assert!(state.lock().await.seeded);
        assert!(agent.try_recv().is_ok(), "a seeded hub pushes again");
    }

    /// The hub also seeds from the other direction: with no host file at all, a
    /// container's announced document is a complete source. The host write and
    /// the push have to agree — a hub that writes a document to disk but stays
    /// muted leaves the host holding content no container is ever told about.
    #[tokio::test]
    async fn a_snapshot_seeds_an_empty_hub() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        // No host file at all.
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        assert!(!state.lock().await.seeded, "precondition");
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"a":{"lastUpdated":"1"}}"#,
            "cella-a",
        )
        .await;

        assert!(
            state.lock().await.seeded,
            "a container's whole document is a complete source"
        );

        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host).expect("host file written"))
                .expect("valid json");
        assert_eq!(on_disk["a"]["lastUpdated"], json!("1"));

        let DaemonMessage::SyncConfigDoc { content, .. } = agent
            .try_recv()
            .expect("the container that seeded the hub must be told what the host now holds")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["a"]["lastUpdated"],
            json!("1")
        );
    }

    /// A hub that read the host file has state of its own, so a reconnecting
    /// container may not re-assert its whole document over it — that is what
    /// patches exist for. The snapshot is dropped and canonical answered.
    #[tokio::test]
    async fn a_host_backed_hub_ignores_a_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        std::fs::write(&host, r#"{"official":{"lastUpdated":"9"}}"#).expect("seed host");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        // A container reconnecting with a stale copy of `official`.
        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"official":{"lastUpdated":"1"}}"#,
            "cella-a",
        )
        .await;

        assert_eq!(
            state.lock().await.canonical,
            json!({"official":{"lastUpdated":"9"}}),
            "a stale snapshot must not revert a host-backed hub"
        );
        let DaemonMessage::SyncConfigDoc { content, .. } = agent
            .try_recv()
            .expect("the sender is still answered with canonical")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["official"]["lastUpdated"],
            json!("9")
        );
    }

    /// Two containers against a host that has no manifest. Nothing seeds the
    /// hub from the host side, so without the announced documents each
    /// container's content stops at the daemon and they stay mutually invisible
    /// for the life of the daemon.
    #[tokio::test]
    async fn a_second_container_learns_what_the_first_seeded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        // No host file at all.
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut a = register_agent(&handles, "cella-a");
        let mut b = register_agent(&handles, "cella-b");

        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"from-a":{"lastUpdated":"1"}}"#,
            "cella-a",
        )
        .await;

        let DaemonMessage::SyncConfigDoc { content, .. } =
            b.try_recv().expect("B must be told about A's content")
        else {
            panic!("expected SyncConfigDoc");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).expect("valid json")["from-a"]["lastUpdated"],
            json!("1")
        );

        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"from-b":{"lastUpdated":"2"}}"#,
            "cella-b",
        )
        .await;

        // A's first message is the echo of its own document; the second carries B.
        a.try_recv().expect("A's own echo");
        let DaemonMessage::SyncConfigDoc { content, .. } =
            a.try_recv().expect("A must be told about B's content")
        else {
            panic!("expected SyncConfigDoc");
        };
        let merged = serde_json::from_str::<serde_json::Value>(&content).expect("valid json");
        assert_eq!(merged["from-a"]["lastUpdated"], json!("1"));
        assert_eq!(merged["from-b"]["lastUpdated"], json!("2"));
        assert_eq!(
            merged.as_object().expect("object").len(),
            2,
            "a snapshot merges into canonical, it does not replace it"
        );
    }

    /// Once seeded from an announced document, a container's ordinary edits flow
    /// again: the delta lands on canonical and reaches the peer.
    #[tokio::test]
    async fn a_seeded_hub_forwards_a_later_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = dir.path().join("known_marketplaces.json");
        let state = Arc::new(Mutex::new(DocSyncState::load(
            Some(&host),
            SyncDoc::KnownMarketplaces,
        )));
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut b = register_agent(&handles, "cella-b");

        on_agent_snapshot(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"a":{"lastUpdated":"1"},"b":{"lastUpdated":"1"}}"#,
            "cella-a",
        )
        .await;
        b.try_recv().expect("the seed reaches B");

        on_agent_change(
            &state,
            &handles,
            Some(&host),
            SyncDoc::KnownMarketplaces,
            r#"{"b":{"lastUpdated":"2"}}"#,
            "cella-a",
        )
        .await;

        let DaemonMessage::SyncConfigDoc { content, .. } =
            b.try_recv().expect("B must be told about A's edit")
        else {
            panic!("expected SyncConfigDoc");
        };
        let pushed = serde_json::from_str::<serde_json::Value>(&content).expect("valid json");
        assert_eq!(pushed["b"]["lastUpdated"], json!("2"), "the edit landed");
        assert_eq!(
            pushed["a"]["lastUpdated"],
            json!("1"),
            "and the key the delta never mentioned is still there"
        );
    }

    /// Revisions must advance with each canonical state so an agent can drop a
    /// push that overtook a newer one in flight.
    #[tokio::test]
    async fn each_canonical_state_gets_a_higher_revision() {
        let state = state_from(json!({}), SyncDoc::KnownMarketplaces);
        let handles: Handles = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = register_agent(&handles, "cella-a");

        for expected in 1..=2u64 {
            on_agent_change(
                &state,
                &handles,
                None,
                SyncDoc::KnownMarketplaces,
                &format!(r#"{{"a":{{"lastUpdated":"{expected}"}}}}"#),
                "cella-a",
            )
            .await;
            let DaemonMessage::SyncConfigDoc { rev, .. } =
                agent.try_recv().expect("agent must be notified")
            else {
                panic!("expected SyncConfigDoc");
            };
            assert_eq!(rev, expected);
        }
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
