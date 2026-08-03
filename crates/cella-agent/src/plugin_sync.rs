//! Write-back sync for Claude Code plugin manifest files.
//!
//! Watches `installed_plugins.json` and `known_marketplaces.json` in the
//! container's plugin directory (backed by tmpfs). When Claude Code modifies
//! these files (plugin install, marketplace refresh), the watcher rewrites
//! container home paths back to the host home and writes the result to the
//! host bind mount at `/tmp/.cella/host-plugins/`.
//!
//! Both paths are pinned by `CELLA_PLUGINS_DIR` / `CELLA_HOST_HOME`, injected
//! at container creation. The agent daemon runs as root, so its `$HOME` is not
//! the remote user's home where the manifests actually live — and the host home
//! cannot be recovered from the manifests themselves, since they may already
//! hold another container's paths.
//!
//! Write-back is event-driven, with no unconditional push at startup: sync is
//! last-writer-wins with no merge, so a boot-time push would let a starting
//! container clobber what another running container had already written. The
//! post-create seed does write into the watched directory, but its result is
//! byte-identical to the host it was seeded from and therefore suppressed —
//! except when the seed normalizes stale paths, which is the intended repair.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Debounce interval — wait this long after the last event before syncing.
const DEBOUNCE: Duration = Duration::from_secs(5);

/// Files that need bidirectional path rewriting.
const SYNC_FILES: &[&str] = &["installed_plugins.json", "known_marketplaces.json"];

/// Host-side plugins directory (bind-mounted at this path inside the container).
const HOST_PLUGINS_DIR: &str = "/tmp/.cella/host-plugins";

/// Resolved inputs for one watcher run.
struct SyncPaths {
    /// Container plugin directory to watch (tmpfs-backed).
    plugins_dir: PathBuf,
    /// Hidden bind mount exposing the host plugin directory.
    host_plugins_dir: PathBuf,
    /// Host home, the rewrite target.
    host_home: PathBuf,
    /// Quiet period after the last event before syncing.
    debounce: Duration,
}

/// Run the plugin manifest sync watcher using paths pinned at container creation.
pub async fn run() {
    let pinned_plugins = env_value("CELLA_PLUGINS_DIR");
    let host_home = env_value("CELLA_HOST_HOME");
    if pinned_plugins.is_none() && host_home.is_none() {
        tracing::debug!("Plugin sync: not configured, skipping");
        return;
    }

    match resolve_sync_paths(pinned_plugins, host_home) {
        Ok(paths) => run_with(paths).await,
        Err(missing) => tracing::warn!(
            "Plugin sync: {missing} is unset; not falling back to $HOME because the agent daemon \
             runs as root while the manifests live under the remote user's home"
        ),
    }
}

/// Build [`SyncPaths`] from the two pinned values, naming the missing one.
///
/// Both are injected together at container creation under the same condition,
/// so exactly one being present means something upstream disagreed — worth a
/// warning rather than a guess. There is deliberately no `$HOME` fallback: for
/// the root-owned agent daemon it resolves to `/root/.claude/plugins`, which is
/// precisely the path that made this sync a silent no-op for four months.
fn resolve_sync_paths(
    plugins_dir: Option<String>,
    host_home: Option<String>,
) -> Result<SyncPaths, &'static str> {
    Ok(SyncPaths {
        plugins_dir: PathBuf::from(plugins_dir.ok_or("CELLA_PLUGINS_DIR")?),
        host_plugins_dir: PathBuf::from(HOST_PLUGINS_DIR),
        host_home: PathBuf::from(host_home.ok_or("CELLA_HOST_HOME")?),
        debounce: DEBOUNCE,
    })
}

/// Watch `paths.plugins_dir` and write each manifest change back to the host.
///
/// Split from [`run`] so the watch → debounce → rewrite → write path can be
/// exercised against temporary directories instead of the process environment
/// and the hardcoded host mount.
async fn run_with(paths: SyncPaths) {
    let SyncPaths {
        plugins_dir,
        host_plugins_dir,
        host_home,
        debounce,
    } = paths;

    if !plugins_dir.is_dir() {
        tracing::warn!(
            "Plugin sync: configured plugins directory is missing: {}",
            plugins_dir.display()
        );
        return;
    }
    if !host_plugins_dir.is_dir() {
        tracing::warn!(
            "Plugin sync: hidden host plugins directory is missing: {}",
            host_plugins_dir.display()
        );
        return;
    }
    let Some(container_home) = plugins_dir.parent().and_then(Path::parent) else {
        tracing::warn!(
            "Plugin sync: cannot derive container home from resolved plugins directory: {}",
            plugins_dir.display()
        );
        return;
    };

    let (tx, mut rx) = mpsc::channel::<()>(16);
    let mut watcher = match create_watcher(tx, plugins_dir.clone()) {
        Ok(watcher) => watcher,
        Err(e) => {
            tracing::warn!(
                "Plugin sync: failed to create watcher for {}: {e}",
                plugins_dir.display()
            );
            return;
        }
    };
    if let Err(e) = watcher.watch(&plugins_dir, RecursiveMode::NonRecursive) {
        tracing::warn!(
            "Plugin sync: failed to watch {}: {e}",
            plugins_dir.display()
        );
        return;
    }

    tracing::info!(
        "Plugin sync: watching {}, host home: {}",
        plugins_dir.display(),
        host_home.display()
    );

    while rx.recv().await.is_some() {
        tokio::time::sleep(debounce).await;
        while rx.try_recv().is_ok() {}
        sync_manifests(&plugins_dir, &host_plugins_dir, container_home, &host_home).await;
    }
}

/// Read an env var, treating an empty value as unset.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Rewrite `{container_home}/.claude` path prefixes to `{host_home}/.claude`.
///
/// Claude Code string-prefix-checks `installPath`/`installLocation`, so the
/// manifests must carry paths that are literally correct for whoever reads them
/// next — the host, in this direction.
fn rewrite_plugin_paths(content: &str, container_home: &Path, host_home: &Path) -> String {
    let from = container_home.join(".claude");
    let to = host_home.join(".claude");
    content.replace(
        from.to_string_lossy().as_ref(),
        to.to_string_lossy().as_ref(),
    )
}

/// Build the inotify watcher that signals `tx` on manifest create/modify.
///
/// `plugins_dir` is carried only to name the directory in watcher errors.
fn create_watcher(
    tx: mpsc::Sender<()>,
    plugins_dir: PathBuf,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
        Ok(event) if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) => {
            let is_sync_file = event.paths.iter().any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| SYNC_FILES.contains(&name))
            });
            if is_sync_file {
                let _ = tx.blocking_send(());
            }
        }
        Err(e) => tracing::warn!(
            "Plugin sync: watcher error for {}: {e}",
            plugins_dir.display()
        ),
        Ok(_) => {}
    })
}

/// Copy both manifests to the host mount with their paths rewritten.
///
/// A manifest that cannot be read is skipped rather than treated as fatal — the
/// other one still syncs, and the next change re-triggers this.
///
/// Writes whose result already matches the host are skipped. `cella up` seeds
/// these files during post-create setup, i.e. into a directory this watcher is
/// already watching, so without this the seed itself would push its own input
/// straight back to the host on every up.
async fn sync_manifests(
    plugins_dir: &Path,
    host_plugins_dir: &Path,
    container_home: &Path,
    host_home: &Path,
) {
    for &file in SYNC_FILES {
        let src = plugins_dir.join(file);
        let dst = host_plugins_dir.join(file);

        let Ok(content) = tokio::fs::read_to_string(&src).await else {
            continue;
        };
        let rewritten = rewrite_plugin_paths(&content, container_home, host_home);
        if tokio::fs::read_to_string(&dst)
            .await
            .is_ok_and(|current| current == rewritten)
        {
            continue;
        }

        if let Err(e) = tokio::fs::write(&dst, rewritten).await {
            tracing::warn!("Plugin sync: failed to write {}: {e}", dst.display());
        } else {
            tracing::trace!("Plugin sync: synced {file} back to host");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this whole change exists for: the daemon runs as root, so
    /// the watched directory must come from the pinned value verbatim and never
    /// from the agent's own `$HOME`.
    #[test]
    fn pinned_paths_are_used_verbatim() {
        let paths = resolve_sync_paths(
            Some("/home/vscode/.claude/plugins".to_string()),
            Some("/Users/alice".to_string()),
        )
        .expect("both pinned values present");
        assert_eq!(
            paths.plugins_dir,
            PathBuf::from("/home/vscode/.claude/plugins")
        );
        assert_eq!(paths.host_home, PathBuf::from("/Users/alice"));
        assert_eq!(paths.host_plugins_dir, PathBuf::from(HOST_PLUGINS_DIR));
    }

    #[test]
    fn missing_plugins_dir_is_named_not_guessed() {
        assert_eq!(
            resolve_sync_paths(None, Some("/Users/alice".to_string())).err(),
            Some("CELLA_PLUGINS_DIR")
        );
    }

    #[test]
    fn missing_host_home_is_named() {
        assert_eq!(
            resolve_sync_paths(Some("/home/vscode/.claude/plugins".to_string()), None).err(),
            Some("CELLA_HOST_HOME")
        );
    }

    #[test]
    fn rewrite_plugin_paths_replaces_single_occurrence() {
        let content = r#"{"installPath":"/home/vscode/.claude/plugins/cache/foo"}"#;
        assert_eq!(
            rewrite_plugin_paths(
                content,
                Path::new("/home/vscode"),
                Path::new("/Users/alice")
            ),
            r#"{"installPath":"/Users/alice/.claude/plugins/cache/foo"}"#
        );
    }

    #[test]
    fn rewrite_plugin_paths_replaces_multiple_occurrences() {
        let content = "/home/vscode/.claude/a /home/vscode/.claude/b";
        assert_eq!(
            rewrite_plugin_paths(
                content,
                Path::new("/home/vscode"),
                Path::new("/Users/alice")
            ),
            "/Users/alice/.claude/a /Users/alice/.claude/b"
        );
    }

    /// End-to-end guard for the wiring itself: a manifest written in the
    /// container directory must land on the host mount with host paths. Unit
    /// tests of the helpers cannot catch a watcher pointed at the wrong
    /// directory, which is exactly how this sync silently did nothing.
    #[tokio::test]
    async fn manifest_change_reaches_host_with_rewritten_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let container_home = tmp.path().join("home/vscode");
        let plugins_dir = container_home.join(".claude/plugins");
        let host_home = tmp.path().join("Users/alice");
        let host_plugins_dir = host_home.join(".claude/plugins");
        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");
        std::fs::create_dir_all(&host_plugins_dir).expect("create host plugins dir");

        tokio::spawn(run_with(SyncPaths {
            plugins_dir: plugins_dir.clone(),
            host_plugins_dir: host_plugins_dir.clone(),
            host_home: host_home.clone(),
            debounce: Duration::from_millis(20),
        }));
        // Let the watcher register before the write it must observe.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let manifest = plugins_dir.join("known_marketplaces.json");
        let written = format!(
            r#"{{"m":{{"installLocation":"{}/.claude/plugins/marketplaces/m"}}}}"#,
            container_home.display()
        );
        std::fs::write(&manifest, &written).expect("write manifest");

        let expected = format!(
            r#"{{"m":{{"installLocation":"{}/.claude/plugins/marketplaces/m"}}}}"#,
            host_home.display()
        );
        let synced = host_plugins_dir.join("known_marketplaces.json");
        for _ in 0..100 {
            if std::fs::read_to_string(&synced).is_ok_and(|c| c == expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "manifest never synced to {}; got {:?}",
            synced.display(),
            std::fs::read_to_string(&synced).ok()
        );
    }

    /// `cella up` seeds the manifests into the directory this watcher is already
    /// watching, so a seed must not push its own input back to the host.
    #[tokio::test]
    async fn identical_content_is_not_rewritten_to_host() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let container_home = tmp.path().join("home/vscode");
        let plugins_dir = container_home.join(".claude/plugins");
        let host_home = tmp.path().join("Users/alice");
        let host_plugins_dir = host_home.join(".claude/plugins");
        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");
        std::fs::create_dir_all(&host_plugins_dir).expect("create host plugins dir");

        let host_file = host_plugins_dir.join("installed_plugins.json");
        let host_content = format!(r#"{{"p":"{}/.claude/plugins/p"}}"#, host_home.display());
        std::fs::write(&host_file, &host_content).expect("write host manifest");
        let before = std::fs::metadata(&host_file)
            .and_then(|m| m.modified())
            .expect("host mtime");

        // The container-side equivalent of the same state, as the seed writes it.
        std::fs::write(
            plugins_dir.join("installed_plugins.json"),
            format!(
                r#"{{"p":"{}/.claude/plugins/p"}}"#,
                container_home.display()
            ),
        )
        .expect("write container manifest");

        sync_manifests(&plugins_dir, &host_plugins_dir, &container_home, &host_home).await;

        assert_eq!(
            std::fs::read_to_string(&host_file).expect("read host manifest"),
            host_content
        );
        assert_eq!(
            std::fs::metadata(&host_file)
                .and_then(|m| m.modified())
                .expect("host mtime"),
            before,
            "host manifest must not be rewritten when the result is identical"
        );
    }

    #[test]
    fn rewrite_plugin_paths_is_noop_without_container_prefix() {
        let content = r#"{"installPath":"/opt/claude/plugins/cache/foo"}"#;
        assert_eq!(
            rewrite_plugin_paths(
                content,
                Path::new("/home/vscode"),
                Path::new("/Users/alice")
            ),
            content
        );
    }
}
