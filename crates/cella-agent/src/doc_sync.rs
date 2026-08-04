//! In-container side of Claude Code document sync.
//!
//! Three documents ride the same machinery: `~/.claude.json` and the two plugin
//! manifests (`installed_plugins.json`, `known_marketplaces.json`), each with a
//! watcher/writer pair sharing one content hash for loop suppression.
//!
//! - **watcher** (container → daemon): on a debounced change, reads the file,
//!   converts it to canonical (host-shaped, and normalized for
//!   `installed_plugins`), diffs it against this container's persisted baseline,
//!   and sends only the resulting [`AgentMessage::ConfigDocPatch`].
//! - **writer** (daemon → container): converts inbound canonical back to this
//!   container's on-disk form and applies it via an atomic `0o600` write.
//!
//! Sending a *patch* rather than the document is what stops a container holding
//! a create-time snapshot from reverting a peer's change: a key it never touched
//! is absent from the patch. The baseline lives in the container
//! ([`BASELINE_DIR`]) rather than in the daemon, so it shares a lifecycle with
//! the manifest the container was seeded with and survives a daemon restart.
//!
//! Agents never write the host mount. The daemon is the sole writer of the host
//! files; with no daemon connection an agent buffers and re-announces on
//! reconnect.
//!
//! The shared `last_hash` records what each half last wrote or sent: the writer
//! updates it after a successful write, the watcher only after a successful send
//! (so a failed send isn't mistaken for already-synced). A matching hash marks
//! the agent's own write and is dropped, preventing a loop.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use cella_env::claude_code::{PathMap, to_canonical, to_local};
use cella_port::CellaPortError;
use cella_protocol::{AgentMessage, SyncDoc};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use crate::reconnecting_client::ReconnectingClient;

/// Debounce for coalescing rapid editor writes.
///
/// Shared by all three documents. The plugin manifests previously used 5s to
/// limit whole-file pushes to the host; with patches there is nothing to limit —
/// an unchanged document yields an empty patch that is never sent, and the
/// content hash absorbs the agent's own writes — so one short interval keeps
/// propagation prompt for every document.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Where each document's last successfully-synced baseline is persisted.
///
/// Container-local: it must share a lifecycle with the seeded manifest, so that
/// a container recreated from the host starts with a baseline matching what was
/// seeded. `/tmp/.cella/` is not on the shared `/cella` volume and not a
/// cella-declared tmpfs (see `state.rs` for the same reasoning).
const BASELINE_DIR: &str = "/tmp/.cella/doc-sync";

/// Canonical content pushed by the daemon, tagged with the document it belongs
/// to. Carried on one channel from the control reader to the writer task.
pub type ApplyMessage = (SyncDoc, String);

/// One document's watcher/writer state.
pub struct DocState {
    doc: SyncDoc,
    /// The container-side file this agent reads and writes.
    path: PathBuf,
    /// Container↔host path translation, `None` for `~/.claude.json` (whose codec
    /// is the identity) and for documents with no resolvable mapping.
    map: Option<PathMap>,
    /// Where [`Self::baseline`] is persisted across agent restarts.
    baseline_path: PathBuf,
    /// Last canonical content this container is known to have contributed or
    /// received. Patches are derived against it.
    baseline: Mutex<serde_json::Value>,
    /// Hash of the raw bytes last written to / read from the container file.
    last_hash: Mutex<String>,
}

impl DocState {
    /// Build a document's state, seeding the baseline from disk.
    ///
    /// An absent persisted baseline falls back to the file's *current* content.
    /// That is deliberately conservative: `diff_merge_patch` only emits `null`
    /// for a key present in the base, so a narrower baseline can never fabricate
    /// a deletion. The worst case of a lost baseline is a local edit that fails
    /// to propagate — never a peer's state being reverted.
    fn new(doc: SyncDoc, path: PathBuf, map: Option<PathMap>, baseline_dir: &Path) -> Self {
        let baseline_path = baseline_dir.join(format!("{}.json", slug(doc)));
        let baseline = load_baseline(&baseline_path)
            .or_else(|| read_canonical(doc, &path, map.as_ref()))
            .unwrap_or_else(|| serde_json::json!({}));
        Self {
            doc,
            baseline_path,
            baseline: Mutex::new(baseline),
            last_hash: Mutex::new(initial_hash(&path)),
            path,
            map,
        }
    }

    /// Record `canonical` as the new baseline, persisting it best-effort.
    async fn set_baseline(&self, canonical: serde_json::Value) {
        if let Some(parent) = self.baseline_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!("doc sync: cannot create {}: {e}", parent.display());
        }
        let bytes = serde_json::to_vec(&canonical).unwrap_or_else(|_| b"{}".to_vec());
        if let Err(e) = cella_filesync::atomic_write(&self.baseline_path, &bytes, 0o600) {
            warn!(
                "doc sync: cannot persist baseline {}: {e}",
                self.baseline_path.display()
            );
        }
        *self.baseline.lock().await = canonical;
    }
}

/// Stable on-disk name for a document's baseline file.
const fn slug(doc: SyncDoc) -> &'static str {
    match doc {
        SyncDoc::ClaudeJson => "claude_json",
        SyncDoc::InstalledPlugins => "installed_plugins",
        SyncDoc::KnownMarketplaces => "known_marketplaces",
    }
}

/// Read a persisted baseline, or `None` when absent or unparseable.
fn load_baseline(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Read a container file straight into canonical form.
fn read_canonical(doc: SyncDoc, path: &Path, map: Option<&PathMap>) -> Option<serde_json::Value> {
    to_canonical(doc, &std::fs::read_to_string(path).ok()?, map)
}

/// Hash of the file's current content, or empty if it doesn't exist yet.
fn initial_hash(path: &Path) -> String {
    std::fs::read(path)
        .ok()
        .map(|b| cella_filesync::sha256_hex(&b))
        .unwrap_or_default()
}

/// Whether this agent participates in Claude Code config sync, per the
/// `CELLA_SYNC_CLAUDE_CONFIG` env var set by the orchestrator at create time.
pub fn sync_enabled() -> bool {
    std::env::var("CELLA_SYNC_CLAUDE_CONFIG").as_deref() == Ok("1")
}

/// Read an env var, treating an empty value as unset.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The exact `~/.claude.json` path to sync (reads process env).
fn config_path() -> Option<PathBuf> {
    resolve_config_path(env_value("CELLA_CLAUDE_JSON_PATH"), env_value("HOME"))
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

/// Build the container↔host [`PathMap`] from the values pinned at create time,
/// naming whichever required one is missing.
///
/// There is deliberately no `$HOME` fallback: for the root-owned agent daemon it
/// resolves to `/root/.claude/plugins`, which is precisely the path that made
/// this sync a silent no-op for four months. The workspace pair is optional — a
/// container whose workspace is not bind-mounted from the host simply gets no
/// `projectPath` translation.
fn resolve_path_map(
    plugins_dir: Option<String>,
    host_home: Option<String>,
    container_workspace: Option<String>,
    host_workspace: Option<String>,
) -> Result<PathMap, &'static str> {
    let plugins_dir = PathBuf::from(plugins_dir.ok_or("CELLA_PLUGINS_DIR")?);
    let host_home = host_home.ok_or("CELLA_HOST_HOME")?;
    let container_claude = plugins_dir
        .parent()
        .ok_or("CELLA_PLUGINS_DIR")?
        .to_string_lossy()
        .into_owned();
    Ok(PathMap {
        claude: (container_claude, format!("{host_home}/.claude")),
        workspace: container_workspace.zip(host_workspace),
    })
}

/// Every document this agent syncs, built once from the process environment.
///
/// Process-global so the watcher/writer tasks and the reconnect re-announce all
/// share one baseline per document rather than racing on the persisted copy.
fn states() -> &'static Vec<Arc<DocState>> {
    static STATES: OnceLock<Vec<Arc<DocState>>> = OnceLock::new();
    STATES.get_or_init(|| build_states(Path::new(BASELINE_DIR)))
}

/// Assemble the per-document states, skipping any whose paths don't resolve.
fn build_states(baseline_dir: &Path) -> Vec<Arc<DocState>> {
    if !sync_enabled() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(path) = config_path() {
        out.push(Arc::new(DocState::new(
            SyncDoc::ClaudeJson,
            path,
            None,
            baseline_dir,
        )));
    } else {
        warn!("doc sync: cannot resolve ~/.claude.json path ($HOME unset)");
    }
    out.extend(plugin_states(baseline_dir));
    out
}

/// The two plugin manifest states, or none when plugin forwarding is off.
fn plugin_states(baseline_dir: &Path) -> Vec<Arc<DocState>> {
    let plugins_dir = env_value("CELLA_PLUGINS_DIR");
    let host_home = env_value("CELLA_HOST_HOME");
    if plugins_dir.is_none() && host_home.is_none() {
        debug!("doc sync: plugin manifests not configured, skipping");
        return Vec::new();
    }
    let dir = PathBuf::from(plugins_dir.clone().unwrap_or_default());
    match resolve_path_map(
        plugins_dir,
        host_home,
        env_value("CELLA_CONTAINER_WORKSPACE"),
        env_value("CELLA_HOST_WORKSPACE"),
    ) {
        Ok(map) => [
            (SyncDoc::InstalledPlugins, "installed_plugins.json"),
            (SyncDoc::KnownMarketplaces, "known_marketplaces.json"),
        ]
        .into_iter()
        .map(|(doc, file)| {
            Arc::new(DocState::new(
                doc,
                dir.join(file),
                Some(map.clone()),
                baseline_dir,
            ))
        })
        .collect(),
        Err(missing) => {
            warn!(
                "doc sync: {missing} is unset; not falling back to $HOME because the agent daemon \
                 runs as root while the manifests live under the remote user's home"
            );
            Vec::new()
        }
    }
}

/// Patches to re-announce to the daemon on every (re)connect, one per document.
///
/// Sent *before* the connection reader starts, so the read happens before any
/// inbound push can clobber local edits. An empty patch is still sent: with no
/// per-container document the daemon cannot tell whether this container is up to
/// date, and its reply carrying canonical is the only way a container that
/// changed nothing while disconnected learns what it missed.
pub async fn reannounce_messages() -> Vec<AgentMessage> {
    let mut out = Vec::new();
    for st in states() {
        let patch = derive_patch(
            st.doc,
            &st.path,
            &*st.baseline.lock().await,
            st.map.as_ref(),
        )
        .await
        .unwrap_or_else(|| serde_json::json!({}));
        out.push(AgentMessage::ConfigDocPatch {
            doc: st.doc,
            patch: patch.to_string(),
        });
    }
    out
}

/// Read `path` and derive the merge patch taking `baseline` to its current
/// content, in canonical form. `None` when the file is unreadable or not JSON —
/// the caller must not treat that as an empty document.
pub async fn derive_patch(
    doc: SyncDoc,
    path: &Path,
    baseline: &serde_json::Value,
    map: Option<&PathMap>,
) -> Option<serde_json::Value> {
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    let canonical = to_canonical(doc, &raw, map)?;
    Some(cella_env::claude_code::diff_merge_patch(
        baseline, &canonical,
    ))
}

/// Spawn a watcher per document plus the shared writer task. `apply_rx` receives
/// canonical content pushed by the daemon (via the control reader).
pub fn spawn(control: &Arc<Mutex<ReconnectingClient>>, apply_rx: mpsc::Receiver<ApplyMessage>) {
    let all = states();
    if all.is_empty() {
        warn!("doc sync: no documents resolved; sync disabled");
        return;
    }
    tokio::spawn(run_writer(apply_rx));
    for st in all {
        tokio::spawn(run_watcher(Arc::clone(st), control.clone()));
    }
}

/// Apply daemon-pushed canonical documents to their container files.
async fn run_writer(mut apply_rx: mpsc::Receiver<ApplyMessage>) {
    while let Some((doc, content)) = apply_rx.recv().await {
        let Some(st) = states().iter().find(|s| s.doc == doc) else {
            debug!("doc sync: dropping {doc:?} push, no state for it");
            continue;
        };
        apply_canonical(st, &content).await;
    }
}

/// Write one inbound canonical document, converted to this container's form.
async fn apply_canonical(st: &DocState, content: &str) {
    let Ok(canonical) = serde_json::from_str::<serde_json::Value>(content) else {
        warn!(
            "doc sync: daemon sent invalid JSON for {:?}; skipping",
            st.doc
        );
        return;
    };
    let local = to_local(st.doc, &canonical, st.map.as_ref());
    let hash = cella_filesync::sha256_hex(local.as_bytes());
    if *st.last_hash.lock().await == hash {
        // Already have this content — but still record the baseline, so a later
        // local edit is diffed against what the daemon believes we hold.
        st.set_baseline(canonical).await;
        return;
    }
    match cella_filesync::atomic_write(&st.path, local.as_bytes(), 0o600) {
        Ok(()) => {
            restore_owner(&st.path);
            // Record the hash only after a successful write so a transient
            // failure doesn't suppress the retry of identical content.
            *st.last_hash.lock().await = hash;
            st.set_baseline(canonical).await;
            debug!(
                "doc sync: applied daemon {:?} to {}",
                st.doc,
                st.path.display()
            );
        }
        Err(e) => warn!("doc sync: failed to write {}: {e}", st.path.display()),
    }
}

/// Restore a file's ownership to whoever owns its parent directory.
///
/// The agent daemon usually runs as root, but `atomic_write` (temp + rename)
/// installs a fresh root-owned inode — which the remote user's `claude` then
/// can't read. Chowning to the directory's owner (best-effort, no-op when the
/// agent already runs as that user) keeps the file readable.
#[cfg(unix)]
fn restore_owner(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else { return };
    let Ok(meta) = std::fs::metadata(parent) else {
        return;
    };
    if let Err(e) = std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid())) {
        warn!(
            "doc sync: could not restore owner on {}: {e}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn restore_owner(_path: &Path) {}

/// Watch one container file and forward its edits to the daemon.
async fn run_watcher(st: Arc<DocState>, control: Arc<Mutex<ReconnectingClient>>) {
    let mut handle = match cella_filesync::watch_file(&st.path, DEBOUNCE) {
        Ok(h) => h,
        Err(e) => {
            warn!("doc sync: cannot watch {}: {e}", st.path.display());
            return;
        }
    };
    debug!("doc sync: watching {}", st.path.display());

    let control = &control;
    while handle.changes.recv().await.is_some() {
        forward_change(
            &st,
            |msg| async move { control.lock().await.send(&msg).await },
        )
        .await;
    }
}

/// Forward a container-side edit to the daemon as a merge patch, advancing
/// `last_hash` and the baseline only on a successful send.
///
/// Recording them *after* a successful send (not before) is the fix for a silent
/// data-loss bug: if the daemon is unreachable, a failed send leaves both
/// unchanged so the edit is re-sent on the next watcher event or on reconnect,
/// instead of being marked already-synced and later clobbered by a stale daemon
/// push. Content whose hash already matches `last_hash` is the agent's own
/// (daemon-applied) write and is skipped without sending.
async fn forward_change<F, Fut>(st: &DocState, send: F)
where
    F: FnOnce(AgentMessage) -> Fut,
    Fut: Future<Output = Result<(), CellaPortError>>,
{
    let Ok(raw) = tokio::fs::read_to_string(&st.path).await else {
        return; // mid-rename or transiently unreadable; next event covers it
    };
    let hash = cella_filesync::sha256_hex(raw.as_bytes());
    if *st.last_hash.lock().await == hash {
        return; // our own (daemon-applied) write — don't echo it back
    }
    let Some(canonical) = to_canonical(st.doc, &raw, st.map.as_ref()) else {
        warn!(
            "doc sync: {} is not valid JSON; skipping",
            st.path.display()
        );
        return;
    };
    let patch = cella_env::claude_code::diff_merge_patch(&*st.baseline.lock().await, &canonical);
    if patch.as_object().is_some_and(serde_json::Map::is_empty) {
        // Reformatted but semantically identical; nothing to send.
        *st.last_hash.lock().await = hash;
        return;
    }
    let msg = AgentMessage::ConfigDocPatch {
        doc: st.doc,
        patch: patch.to_string(),
    };
    match send(msg).await {
        Ok(()) => {
            *st.last_hash.lock().await = hash;
            st.set_baseline(canonical).await;
        }
        Err(e) => warn!(
            "doc sync: failed to send {:?} change to daemon: {e}",
            st.doc
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn test_map() -> PathMap {
        PathMap {
            claude: (
                "/home/vscode/.claude".to_string(),
                "/Users/alice/.claude".to_string(),
            ),
            workspace: None,
        }
    }

    fn doc_state(dir: &Path, doc: SyncDoc, file: &str, map: Option<PathMap>) -> DocState {
        DocState::new(doc, dir.join(file), map, &dir.join("baselines"))
    }

    /// Retargeted from the deleted `plugin_sync`: a container-side manifest
    /// change must produce a patch containing only what changed, in host-shaped
    /// paths. The old code pushed the whole file, which is the bug.
    #[tokio::test]
    async fn container_edit_produces_a_minimal_host_shaped_patch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = tmp.path().join("known_marketplaces.json");
        let baseline: serde_json::Value =
            serde_json::from_str(r#"{"a":{"lastUpdated":"1"},"b":{"lastUpdated":"1"}}"#)
                .expect("valid json");

        std::fs::write(
            &manifest,
            r#"{"a":{"lastUpdated":"2"},"b":{"lastUpdated":"1"}}"#,
        )
        .expect("edit");
        let patch = derive_patch(
            SyncDoc::KnownMarketplaces,
            &manifest,
            &baseline,
            Some(&test_map()),
        )
        .await
        .expect("patch derived");
        assert_eq!(
            patch,
            json!({"a":{"lastUpdated":"2"}}),
            "only the changed key is sent"
        );
    }

    /// Container paths in the manifest must reach the daemon host-shaped.
    #[tokio::test]
    async fn derived_patch_carries_host_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = tmp.path().join("known_marketplaces.json");
        std::fs::write(
            &manifest,
            r#"{"m":{"installLocation":"/home/vscode/.claude/plugins/marketplaces/m"}}"#,
        )
        .expect("seed");
        let patch = derive_patch(
            SyncDoc::KnownMarketplaces,
            &manifest,
            &json!({}),
            Some(&test_map()),
        )
        .await
        .expect("patch derived");
        assert_eq!(
            patch["m"]["installLocation"],
            json!("/Users/alice/.claude/plugins/marketplaces/m")
        );
    }

    /// The create-time seed writes into the directory this watcher watches, so
    /// identical content must produce nothing to send.
    #[tokio::test]
    async fn identical_content_produces_no_patch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = tmp.path().join("known_marketplaces.json");
        std::fs::write(&manifest, r#"{"a":{"lastUpdated":"1"}}"#).expect("seed");
        let patch = derive_patch(
            SyncDoc::KnownMarketplaces,
            &manifest,
            &json!({"a":{"lastUpdated":"1"}}),
            Some(&test_map()),
        )
        .await
        .expect("patch derived");
        assert_eq!(patch, json!({}), "an unchanged file yields an empty patch");
    }

    /// A lost or absent baseline may fail to propagate a local edit, but must
    /// never fabricate a deletion — that is what keeps the degraded case safe
    /// for peers.
    #[tokio::test]
    async fn absent_baseline_patch_contains_no_deletions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = tmp.path().join("known_marketplaces.json");
        std::fs::write(&manifest, r#"{"a":{"lastUpdated":"1"}}"#).expect("seed");
        let patch = derive_patch(SyncDoc::KnownMarketplaces, &manifest, &json!({}), None)
            .await
            .expect("patch derived");
        assert!(
            !patch.to_string().contains("null"),
            "an empty baseline must emit additions only: {patch}"
        );
    }

    #[tokio::test]
    async fn unparseable_file_yields_no_patch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = tmp.path().join("known_marketplaces.json");
        std::fs::write(&manifest, "{not json").expect("seed");
        assert!(
            derive_patch(SyncDoc::KnownMarketplaces, &manifest, &json!({}), None)
                .await
                .is_none(),
            "a corrupt manifest must not be pushed as an empty document"
        );
    }

    #[tokio::test]
    async fn baseline_survives_a_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("known_marketplaces.json"),
            r#"{"a":{"lastUpdated":"1"}}"#,
        )
        .expect("seed");

        // First "run": the agent records a baseline, then goes away.
        let st = doc_state(
            tmp.path(),
            SyncDoc::KnownMarketplaces,
            "known_marketplaces.json",
            None,
        );
        st.set_baseline(json!({"a":{"lastUpdated":"1"}})).await;
        drop(st);

        // Second "run": rebuilt from the persisted baseline, not from `{}`.
        std::fs::write(
            tmp.path().join("known_marketplaces.json"),
            r#"{"a":{"lastUpdated":"2"}}"#,
        )
        .expect("edit");
        let st = doc_state(
            tmp.path(),
            SyncDoc::KnownMarketplaces,
            "known_marketplaces.json",
            None,
        );
        let patch = derive_patch(st.doc, &st.path, &*st.baseline.lock().await, None)
            .await
            .expect("patch derived");
        assert_eq!(
            patch,
            json!({"a":{"lastUpdated":"2"}}),
            "the persisted baseline must be diffed against, not an empty document"
        );
    }

    /// Without a persisted baseline the current file content is used, so a fresh
    /// agent in a seeded container has nothing to announce.
    #[tokio::test]
    async fn absent_baseline_falls_back_to_current_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("known_marketplaces.json"),
            r#"{"a":{"lastUpdated":"1"}}"#,
        )
        .expect("seed");
        let st = doc_state(
            tmp.path(),
            SyncDoc::KnownMarketplaces,
            "known_marketplaces.json",
            None,
        );
        assert_eq!(*st.baseline.lock().await, json!({"a":{"lastUpdated":"1"}}));
    }

    /// A `projectPath` belonging to a *different* container's workspace matches
    /// neither mapping. It must pass through untouched in both directions —
    /// corrupting it would be worse than leaving it inert — and, crucially, an
    /// entry this container never edits must not appear in its patch at all.
    #[tokio::test]
    async fn foreign_workspace_project_path_passes_through_and_is_not_pushed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = tmp.path().join("installed_plugins.json");
        let seeded = r#"{"version":2,"plugins":{"p@m":[
            {"scope":"project","projectPath":"/workspaces/other-repo","version":"1.0"},
            {"scope":"user","version":"1.0"}
        ]}}"#;
        std::fs::write(&manifest, seeded).expect("seed");
        let map = PathMap {
            claude: (
                "/home/vscode/.claude".to_string(),
                "/Users/alice/.claude".to_string(),
            ),
            workspace: Some((
                "/workspaces/cella".to_string(),
                "/Users/alice/src/cella".to_string(),
            )),
        };
        let st = DocState::new(
            SyncDoc::InstalledPlugins,
            manifest.clone(),
            Some(map.clone()),
            &tmp.path().join("baselines"),
        );

        // The seed is the baseline, so a subsequent edit to the *user* entry
        // alone must not carry the foreign project entry along.
        std::fs::write(
            &manifest,
            r#"{"version":2,"plugins":{"p@m":[
                {"scope":"project","projectPath":"/workspaces/other-repo","version":"1.0"},
                {"scope":"user","version":"2.0"}
            ]}}"#,
        )
        .expect("edit");
        let patch = derive_patch(st.doc, &st.path, &*st.baseline.lock().await, Some(&map))
            .await
            .expect("patch derived");
        assert_eq!(
            patch,
            json!({"plugins":{"p@m":{"user":{"version":"2.0"}}}}),
            "an untouched foreign-workspace entry must not be pushed: {patch}"
        );
    }

    #[tokio::test]
    async fn forward_change_keeps_hash_when_send_fails() {
        // The fix for silent offline-edit loss: a failed send must NOT advance
        // the hash, so the edit stays eligible for re-send on the next event or
        // on reconnect (rather than being marked synced and later clobbered).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(".claude.json"), r#"{"a":1}"#).expect("seed");
        let st = doc_state(tmp.path(), SyncDoc::ClaudeJson, ".claude.json", None);
        *st.baseline.lock().await = json!({});
        *st.last_hash.lock().await = String::new();

        forward_change(&st, |_msg| async {
            Err(CellaPortError::ControlSocket {
                message: "daemon down".to_string(),
            })
        })
        .await;
        assert!(
            st.last_hash.lock().await.is_empty(),
            "a failed send must not advance last_hash"
        );
    }

    #[tokio::test]
    async fn forward_change_advances_hash_and_baseline_on_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(".claude.json"), r#"{"a":1}"#).expect("seed");
        let st = doc_state(tmp.path(), SyncDoc::ClaudeJson, ".claude.json", None);
        *st.baseline.lock().await = json!({});
        *st.last_hash.lock().await = String::new();

        forward_change(&st, |_msg| async { Ok(()) }).await;
        assert_eq!(
            *st.last_hash.lock().await,
            cella_filesync::sha256_hex(br#"{"a":1}"#)
        );
        assert_eq!(*st.baseline.lock().await, json!({"a":1}));
    }

    #[tokio::test]
    async fn forward_change_skips_unchanged_content() {
        // Content whose hash already matches is the agent's own daemon-applied
        // write; it must not be sent back, preventing an echo loop.
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(".claude.json"), r#"{"a":1}"#).expect("seed");
        let st = doc_state(tmp.path(), SyncDoc::ClaudeJson, ".claude.json", None);
        let hash = cella_filesync::sha256_hex(br#"{"a":1}"#);
        *st.last_hash.lock().await = hash.clone();

        let sent = Arc::new(AtomicBool::new(false));
        let sent_in_closure = sent.clone();
        forward_change(&st, |_msg| {
            sent_in_closure.store(true, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await;
        assert!(
            !sent.load(Ordering::SeqCst),
            "matching content must not be sent"
        );
        assert_eq!(*st.last_hash.lock().await, hash, "hash must stay unchanged");
    }

    /// A corrupt container manifest must be skipped, not pushed as an empty
    /// document that would wipe canonical for every peer.
    #[tokio::test]
    async fn forward_change_skips_invalid_json() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(".claude.json"), "{not json").expect("seed");
        let st = doc_state(tmp.path(), SyncDoc::ClaudeJson, ".claude.json", None);
        *st.last_hash.lock().await = String::new();

        let sent = Arc::new(AtomicBool::new(false));
        let sent_in_closure = sent.clone();
        forward_change(&st, |_msg| {
            sent_in_closure.store(true, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await;
        assert!(
            !sent.load(Ordering::SeqCst),
            "invalid JSON must not be forwarded"
        );
    }

    #[tokio::test]
    async fn apply_canonical_writes_container_shaped_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let st = doc_state(
            tmp.path(),
            SyncDoc::KnownMarketplaces,
            "known_marketplaces.json",
            Some(test_map()),
        );
        apply_canonical(
            &st,
            r#"{"m":{"installLocation":"/Users/alice/.claude/plugins/marketplaces/m"}}"#,
        )
        .await;

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&st.path).expect("written"))
                .expect("valid json");
        assert_eq!(
            written["m"]["installLocation"],
            json!("/home/vscode/.claude/plugins/marketplaces/m"),
            "the container's copy must carry paths literally correct for it"
        );
    }

    /// `installed_plugins.json` must land on disk in its real entry-array
    /// schema; the context-keyed form is wire-only.
    #[tokio::test]
    async fn apply_canonical_restores_entry_arrays() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let st = doc_state(
            tmp.path(),
            SyncDoc::InstalledPlugins,
            "installed_plugins.json",
            Some(test_map()),
        );
        apply_canonical(
            &st,
            r#"{"version":2,"plugins":{"p@m":{"user":{"scope":"user","installPath":"/Users/alice/.claude/plugins/cache/p"}}}}"#,
        )
        .await;

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&st.path).expect("written"))
                .expect("valid json");
        let entries = written["plugins"]["p@m"]
            .as_array()
            .expect("on-disk form is an array");
        assert_eq!(
            entries[0]["installPath"],
            json!("/home/vscode/.claude/plugins/cache/p")
        );
    }

    #[test]
    fn path_map_uses_pinned_values_verbatim() {
        let map = resolve_path_map(
            Some("/home/vscode/.claude/plugins".into()),
            Some("/Users/alice".into()),
            Some("/workspaces/cella".into()),
            Some("/Users/alice/src/cella".into()),
        )
        .expect("both pinned values present");
        assert_eq!(
            map.claude,
            (
                "/home/vscode/.claude".to_string(),
                "/Users/alice/.claude".to_string()
            )
        );
        assert_eq!(
            map.workspace,
            Some((
                "/workspaces/cella".to_string(),
                "/Users/alice/src/cella".to_string()
            ))
        );
    }

    #[test]
    fn missing_workspace_vars_leave_the_map_partial_not_absent() {
        let map = resolve_path_map(
            Some("/home/vscode/.claude/plugins".into()),
            Some("/Users/alice".into()),
            None,
            None,
        )
        .expect("claude mapping still resolvable");
        assert!(map.workspace.is_none());
    }

    #[test]
    fn missing_plugins_dir_is_named_not_guessed() {
        assert_eq!(
            resolve_path_map(None, Some("/Users/alice".into()), None, None).err(),
            Some("CELLA_PLUGINS_DIR")
        );
    }

    #[test]
    fn missing_host_home_is_named() {
        assert_eq!(
            resolve_path_map(
                Some("/home/vscode/.claude/plugins".into()),
                None,
                None,
                None
            )
            .err(),
            Some("CELLA_HOST_HOME")
        );
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
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(initial_hash(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn initial_hash_matches_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, b"{\"a\":1}").expect("seed");
        assert_eq!(
            initial_hash(&path),
            cella_filesync::sha256_hex(b"{\"a\":1}")
        );
    }

    #[tokio::test]
    async fn writer_skips_when_hash_already_matches() {
        // Guards the loop: content the agent already has (same hash) is not
        // re-written, so a daemon echo of our own edit is a no-op.
        let tmp = tempfile::tempdir().expect("tempdir");
        let st = doc_state(tmp.path(), SyncDoc::ClaudeJson, ".claude.json", None);
        let content = to_local(SyncDoc::ClaudeJson, &json!({"a":1}), None);
        *st.last_hash.lock().await = cella_filesync::sha256_hex(content.as_bytes());

        apply_canonical(&st, r#"{"a":1}"#).await;
        assert!(
            !st.path.exists(),
            "writer must skip a write whose hash already matches"
        );
        assert_eq!(
            *st.baseline.lock().await,
            json!({"a":1}),
            "a suppressed write must still record the baseline"
        );
    }
}
