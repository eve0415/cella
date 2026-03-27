//! Bidirectional sync for Claude Code plugin manifest files.
//!
//! Watches `installed_plugins.json` and `known_marketplaces.json` in the
//! container's `~/.claude/plugins/` directory (backed by tmpfs). When Claude
//! Code modifies these files (plugin install, marketplace refresh), the watcher
//! reverse-rewrites home paths and writes the result to the host bind mount
//! at `/tmp/.cella/host-plugins/`.

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

/// Run the plugin manifest sync watcher.
///
/// Watches the container's plugin manifest files and reverse-rewrites paths
/// back to the host on every change. Runs until the channel closes (container
/// shutdown).
pub async fn run(container_home: String, host_home: String) {
    let plugins_dir = format!("{container_home}/.claude/plugins");

    if !Path::new(&plugins_dir).exists() || !Path::new(HOST_PLUGINS_DIR).exists() {
        tracing::debug!("Plugin sync: paths not ready, skipping");
        return;
    }

    let (tx, mut rx) = mpsc::channel::<()>(16);

    let watch_dir = plugins_dir.clone();
    let mut watcher = match notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        if let Ok(event) = res
            && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
        {
            let dominated = event.paths.iter().any(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| SYNC_FILES.contains(&name))
            });
            if dominated {
                let _ = tx.blocking_send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("Plugin sync: failed to create watcher: {e}");
            return;
        }
    };

    if let Err(e) = watcher.watch(Path::new(&watch_dir), RecursiveMode::NonRecursive) {
        tracing::warn!("Plugin sync: failed to watch {watch_dir}: {e}");
        return;
    }

    tracing::debug!("Plugin sync: watching {watch_dir}");

    let from_prefix = format!("{container_home}/.claude");
    let to_prefix = format!("{host_home}/.claude");

    loop {
        // Wait for an event
        if rx.recv().await.is_none() {
            break;
        }

        // Debounce: drain buffered events and wait
        tokio::time::sleep(DEBOUNCE).await;
        while rx.try_recv().is_ok() {}

        // Reverse-rewrite and sync each file
        for &file in SYNC_FILES {
            let src = PathBuf::from(&plugins_dir).join(file);
            let dst = PathBuf::from(HOST_PLUGINS_DIR).join(file);

            let Ok(content) = tokio::fs::read_to_string(&src).await else {
                continue;
            };

            let rewritten = content.replace(&from_prefix, &to_prefix);

            if let Err(e) = tokio::fs::write(&dst, rewritten).await {
                tracing::warn!("Plugin sync: failed to write {file} to host: {e}");
            } else {
                tracing::debug!("Plugin sync: synced {file} back to host");
            }
        }
    }
}
