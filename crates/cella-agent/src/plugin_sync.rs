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

/// Detect the original home path from a host JSON file by finding `/.claude/`
/// path references. Returns the home portion (e.g., `/home/node`).
fn detect_home_from_json(content: &str) -> Option<String> {
    // Look for patterns like "/home/USER/.claude/" or "/Users/USER/.claude/"
    // or "/root/.claude/" in JSON string values
    for line in content.lines() {
        // Find /.claude/ and extract the home prefix
        if let Some(idx) = line.find("/.claude/") {
            // Walk backwards to find the start of the path (after a quote)
            let prefix = &line[..idx];
            if let Some(quote_idx) = prefix.rfind('"') {
                let home = &prefix[quote_idx + 1..];
                if home.starts_with('/') {
                    return Some(home.to_string());
                }
            }
        }
    }
    None
}

/// Run the plugin manifest sync watcher.
///
/// Watches the container's plugin manifest files and reverse-rewrites paths
/// back to the host on every change. Detects the original host home path from
/// the host's JSON files at startup.
pub async fn run(container_home: String) {
    let plugins_dir = format!("{container_home}/.claude/plugins");

    if !Path::new(&plugins_dir).exists() || !Path::new(HOST_PLUGINS_DIR).exists() {
        tracing::debug!("Plugin sync: paths not ready, skipping");
        return;
    }

    // Detect the original home path from host JSON files
    let host_home = detect_host_home().await;
    let Some(host_home) = host_home else {
        tracing::debug!("Plugin sync: could not detect host home path, skipping");
        return;
    };

    let (tx, mut rx) = mpsc::channel::<()>(16);

    let watch_dir = plugins_dir.clone();
    let mut watcher = match notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        if let Ok(event) = res
            && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
        {
            let is_sync_file = event.paths.iter().any(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| SYNC_FILES.contains(&name))
            });
            if is_sync_file {
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

    tracing::debug!("Plugin sync: watching {watch_dir}, host home: {host_home}");

    let from_prefix = format!("{container_home}/.claude");
    let to_prefix = format!("{host_home}/.claude");

    loop {
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

/// Detect the original host home path from any host JSON in the hidden mount.
async fn detect_host_home() -> Option<String> {
    for &file in SYNC_FILES {
        let path = PathBuf::from(HOST_PLUGINS_DIR).join(file);
        if let Ok(content) = tokio::fs::read_to_string(&path).await
            && let Some(home) = detect_home_from_json(&content)
        {
            return Some(home);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_home_linux() {
        let json = r#"{"installPath": "/home/node/.claude/plugins/cache/foo"}"#;
        assert_eq!(detect_home_from_json(json), Some("/home/node".to_string()));
    }

    #[test]
    fn detect_home_macos() {
        let json = r#"{"path": "/Users/alice/.claude/plugins"}"#;
        assert_eq!(
            detect_home_from_json(json),
            Some("/Users/alice".to_string())
        );
    }

    #[test]
    fn detect_home_root() {
        let json = r#"{"installLocation": "/root/.claude/plugins/marketplaces/foo"}"#;
        assert_eq!(detect_home_from_json(json), Some("/root".to_string()));
    }

    #[test]
    fn detect_home_no_match() {
        let json = r#"{"name": "no paths here"}"#;
        assert_eq!(detect_home_from_json(json), None);
    }
}
