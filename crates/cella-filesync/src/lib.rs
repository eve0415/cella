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

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

/// Per-process counter making each temp file name unique, so concurrent [`temp_name`] calls from the *same* process (e.g. the daemon's host watcher plus several per-agent handlers) never share a temp inode.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Whether [`atomic_write`] gave the file the owner it was asked for.
#[derive(Debug)]
pub enum Ownership {
    /// No owner was requested; the file belongs to the writer.
    Unrequested,
    /// The requested owner was set.
    Applied,
    /// The requested owner could not be set (a user-namespaced or rootless
    /// runtime, NFS `root_squash`), so the file belongs to the writer.
    Failed(std::io::Error),
}

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
/// Writes a fresh sibling temp file, sets its mode (and owner, when given) on
/// the open handle, then renames it over `path`. Readers therefore never
/// observe a partially-written file, and each write installs a fresh inode (so
/// a concurrent parent-directory watcher reliably sees a create/rename event).
///
/// The temp file is created exclusively, so a file or symlink already at its
/// name is never written through, and a symlink at `path` is replaced rather
/// than followed. Such a leftover at the temp name, for example one that a
/// killed writer with the same pid left behind, is unlinked once (never
/// followed) and the exclusive create retried.
///
/// `mode` (e.g. `0o600`) and `owner` (uid, gid) are applied on Unix and
/// ignored elsewhere. An owner that cannot be set does not fail the write: the
/// file is installed with the writer's ownership and the error is returned in
/// [`Ownership::Failed`] for the caller to report.
///
/// # Errors
///
/// Returns any I/O error from creating, writing, or renaming the temp file.
pub fn atomic_write(
    path: &Path,
    contents: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
) -> std::io::Result<Ownership> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("config"));
    // Sibling temp in the same directory guarantees the rename is same-fs and therefore atomic.
    let tmp = dir.join(temp_name(file_name));
    write_through_temp(&tmp, path, contents, mode, owner)
}

/// A fresh name for a temp file to be renamed over `file_name` in the same directory.
///
/// The pid + per-process sequence suffix keeps it unique across concurrent writers in the same process and across processes, so no two writers ever share a temp inode.
#[must_use]
pub fn temp_name(file_name: &OsStr) -> OsString {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".cella-tmp.{}.{seq}", std::process::id()));
    name
}

/// Create `tmp` exclusively, fill it, and rename it over `path`. Removes `tmp`
/// on failure only once this call has created it.
fn write_through_temp(
    tmp: &Path,
    path: &Path,
    contents: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
) -> std::io::Result<Ownership> {
    let mut file = create_temp(tmp, mode)?;

    let write_result = (|| -> std::io::Result<Ownership> {
        file.write_all(contents)?;
        file.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
            Ok(owner.map_or(Ownership::Unrequested, |(uid, gid)| {
                std::os::unix::fs::fchown(&file, Some(uid), Some(gid))
                    .map_or_else(Ownership::Failed, |()| Ownership::Applied)
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = (mode, owner);
            Ok(Ownership::Unrequested)
        }
    })();
    drop(file);

    let ownership = match write_result {
        Ok(ownership) => ownership,
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            return Err(e);
        }
    };

    std::fs::rename(tmp, path)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(tmp);
        })
        .map(|()| ownership)
}

/// Create `tmp` exclusively with `mode`. Whatever already sits at that name is
/// unlinked once, without following it, and the exclusive create retried.
fn create_temp(tmp: &Path, mode: u32) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    match options.open(tmp) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(tmp)?;
            options.open(tmp)
        }
        opened => opened,
    }
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
    let target_name = path.file_name().map(OsString::from);

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
        atomic_write(&path, b"{\"a\":1}", 0o600, None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_write(&path, b"old", 0o600, None).unwrap();
        atomic_write(&path, b"new", 0o600, None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_write(&path, b"x", 0o600, None).unwrap();
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
        atomic_write(&path, b"x", 0o600, None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_replaces_a_symlink_left_at_the_temp_name_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let path = dir.path().join("config.json");
        let tmp = dir.path().join(".config.json.cella-tmp.planted");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();

        write_through_temp(&tmp, &path, b"new", 0o600, None).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(std::fs::symlink_metadata(&tmp).is_err());
    }

    /// A killed writer whose pid comes back after a container restart leaves a
    /// temp under the very name the next write picks.
    #[test]
    fn atomic_write_replaces_a_stale_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let tmp = dir.path().join(".config.json.cella-tmp.stale");
        std::fs::write(&tmp, b"half-written by a killed writer").unwrap();

        write_through_temp(&tmp, &path, b"new", 0o600, None).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!tmp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_replaces_a_symlink_at_the_path_instead_of_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let path = dir.path().join("config.json");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        atomic_write(&path, b"new", 0o600, None).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    /// A group other than `primary` that this process may chown a file it
    /// owns to: any gid as root, otherwise one of its supplementary groups.
    #[cfg(target_os = "linux")]
    fn other_permitted_gid(uid: u32, primary: u32) -> Option<u32> {
        if uid == 0 {
            return Some(primary.wrapping_add(4242));
        }
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        status
            .lines()
            .find_map(|line| line.strip_prefix("Groups:"))?
            .split_whitespace()
            .filter_map(|g| g.parse().ok())
            .find(|&g| g != primary)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_write_applies_the_requested_owner() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe");
        std::fs::write(&probe, b"").unwrap();
        let meta = std::fs::metadata(&probe).unwrap();
        let (uid, default_gid) = (meta.uid(), meta.gid());
        let Some(gid) = other_permitted_gid(uid, default_gid) else {
            eprintln!("skipped: no group other than {default_gid} to chown to");
            return;
        };
        let path = dir.path().join("config.json");

        let ownership = atomic_write(&path, b"x", 0o600, Some((uid, gid))).unwrap();

        assert!(matches!(ownership, Ownership::Applied), "{ownership:?}");
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (uid, gid));
    }

    /// Rootless and user-namespaced runtimes refuse the chown; the old
    /// path-based owner restore only warned there, and so must this.
    #[cfg(unix)]
    #[test]
    fn atomic_write_installs_the_file_when_the_owner_cannot_be_set() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe");
        std::fs::write(&probe, b"").unwrap();
        let own_uid = std::fs::metadata(&probe).unwrap().uid();
        if own_uid == 0 {
            eprintln!("skipped: root may chown to any owner");
            return;
        }
        let path = dir.path().join("config.json");

        let ownership = atomic_write(&path, b"new", 0o600, Some((0, 0))).unwrap();

        assert!(matches!(ownership, Ownership::Failed(_)), "{ownership:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&path).unwrap().uid(), own_uid);
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
        atomic_write(&path, b"replaced", 0o600, None).unwrap();

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
