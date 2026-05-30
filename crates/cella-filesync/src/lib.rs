//! Small filesystem-sync primitives shared by the host daemon and the
//! in-container agent for bidirectional single-file config sync.
//!
//! Three concerns, deliberately generic (no Claude-specific logic — that lives
//! in `cella-env`):
//! - [`sha256_hex`] — content hashing for loop-suppression guards.
//! - [`atomic_write`] — temp-write + rename so readers never see a half-written
//!   file (and so the file is replaced as a fresh inode each time).
//! - [`watch_file`] — a debounced single-file watcher that watches the *parent
//!   directory* (non-recursive) filtered to the file name, so it survives an
//!   atomic replace (the replacement is a new inode the old watch would miss).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

/// Per-process counter making each temp file name unique, so concurrent
/// [`atomic_write`] calls from the *same* process (e.g. the daemon's host
/// watcher plus several per-agent handlers) never share a temp inode.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Lowercase hex SHA-256 of `bytes`. Used as a content fingerprint for the
/// loop-suppression guards on both sync sides.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Write `contents` to `path` atomically via temp-write + `rename`.
///
/// Writes a sibling temp file, sets its mode, then renames it over `path`.
/// Readers therefore never observe a partially-written file, and each write
/// installs a fresh inode (so a concurrent parent-directory watcher reliably
/// sees a create/rename event).
///
/// `mode` is applied on Unix (e.g. `0o600`); ignored elsewhere.
///
/// # Errors
///
/// Returns any I/O error from creating, writing, or renaming the temp file.
pub fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    // Sibling temp in the same directory guarantees the rename is same-fs and
    // therefore atomic. The pid + per-process sequence suffix keeps the temp
    // name unique across concurrent writers in the same process and across
    // processes, so no two writers ever share a temp inode.
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{file_name}.cella-tmp.{}.{seq}",
        std::process::id()
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A live single-file watcher. Drop it to stop watching.
///
/// `changes` yields one `()` per debounced burst of create/modify events for
/// the watched file. The receiver closes when the handle is dropped.
pub struct FileWatchHandle {
    /// Debounced change notifications for the watched file.
    pub changes: mpsc::Receiver<()>,
    /// Kept alive so the underlying OS watch stays registered.
    _watcher: RecommendedWatcher,
}

/// Watch a single file for create/modify events, debounced by `debounce`.
///
/// Watches the file's *parent directory* (non-recursive) and filters events to
/// the target file name. This is what lets the watch survive an atomic replace:
/// `rename`-ing a new inode over the path is a directory event the parent watch
/// still receives, whereas a watch bound to the original inode would go stale.
///
/// # Errors
///
/// Returns a [`notify::Error`] if the OS watch cannot be established (e.g. the
/// parent directory does not exist).
pub fn watch_file(path: &Path, debounce: Duration) -> notify::Result<FileWatchHandle> {
    let dir = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let target_name = path.file_name().map(std::ffi::OsString::from);

    // notify runs its callback on its own thread, so `blocking_send` is safe.
    let (raw_tx, mut raw_rx) = mpsc::channel::<()>(16);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        let hit = event
            .paths
            .iter()
            .any(|p| p.file_name() == target_name.as_deref());
        if hit {
            let _ = raw_tx.blocking_send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    // Debounce: coalesce a burst of raw events into a single notification.
    let (out_tx, out_rx) = mpsc::channel::<()>(8);
    tokio::spawn(async move {
        while raw_rx.recv().await.is_some() {
            tokio::time::sleep(debounce).await;
            while raw_rx.try_recv().is_ok() {}
            if out_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    Ok(FileWatchHandle {
        changes: out_rx,
        _watcher: watcher,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_hex_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn atomic_write_creates_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_write(&path, b"{\"a\":1}", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_write(&path, b"old", 0o600).unwrap();
        atomic_write(&path, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_write(&path, b"x", 0o600).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        atomic_write(&path, b"x", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[tokio::test]
    async fn watch_file_detects_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.json");
        std::fs::write(&path, b"initial").unwrap();

        let mut handle = watch_file(&path, Duration::from_millis(100)).unwrap();
        // Modify the file after the watch is established.
        std::fs::write(&path, b"changed").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), handle.changes.recv()).await;
        assert!(got.is_ok(), "watcher did not fire within 3s");
        assert!(
            got.unwrap().is_some(),
            "watcher channel closed unexpectedly"
        );
    }

    #[tokio::test]
    async fn watch_file_survives_atomic_replace() {
        // The whole reason for watching the parent dir: an atomic replace
        // (temp + rename) swaps the inode, which a file-level watch would miss.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.json");
        std::fs::write(&path, b"initial").unwrap();

        let mut handle = watch_file(&path, Duration::from_millis(100)).unwrap();
        atomic_write(&path, b"replaced", 0o600).unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), handle.changes.recv()).await;
        assert!(
            got.is_ok(),
            "watcher did not fire on atomic replace within 3s"
        );
        assert!(
            got.unwrap().is_some(),
            "watcher channel closed unexpectedly"
        );
    }
}
