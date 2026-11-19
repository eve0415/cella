//! Single-instance lock for `cella-agent daemon`.
//!
//! More than one daemon can be launched in a container: an entrypoint loop
//! from an older cella next to the supervisor `cella up` starts, or two
//! overlapping restarts. Every daemon takes an exclusive `flock` on one file
//! before doing anything else, so only one of them binds the proxy and talks
//! to the host daemon while the rest wait and take over when it exits.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use tracing::warn;

/// Take the lock for a daemon that runs as root (`is_root`) or not.
///
/// A root daemon that cannot open or lock the file warns and runs without the
/// lock (`Ok(None)`): it is the daemon that must serve the proxy, so an
/// unusable lock must not take the proxy down with it. A non-root daemon gets
/// the error back and must exit.
pub fn take(path: &Path, is_root: bool) -> io::Result<Option<File>> {
    match acquire(path, is_root) {
        Ok(file) => Ok(Some(file)),
        Err(e) if is_root => {
            warn!("Running without the agent lock at {}: {e}", path.display());
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Open the lock file at `path` and block until this process holds an
/// exclusive lock on it. The returned `File` must live as long as the daemon.
///
/// Only a root daemon passes `create`, which creates the parent directory and
/// a 0600 lock file. A non-root daemon opens an existing file only, so it
/// fails on a root-owned lock and never leaves behind a lock or directory it
/// owns that would let it win the lock against the root daemon. A symlink at
/// `path` is never followed.
fn acquire(path: &Path, create: bool) -> io::Result<File> {
    if create && let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::TryLockError;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn second_handle_cannot_lock_until_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("agent.lock");

        let held = acquire(&path, true).unwrap();
        let other = File::open(&path).unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));

        drop(held);
        other.try_lock().unwrap();
    }

    #[test]
    fn created_lock_file_is_private_to_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.lock");
        let _held = acquire(&path, true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn without_create_a_missing_lock_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("agent.lock");
        let err = acquire(&path, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn without_create_an_existing_lock_file_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.lock");
        File::create(&path).unwrap();
        let _held = acquire(&path, false).unwrap();
        let other = File::open(&path).unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
    }

    #[test]
    fn a_symlink_at_the_lock_path_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        let path = dir.path().join("agent.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        assert!(acquire(&path, true).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn root_runs_unlocked_when_the_lock_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        File::create(&not_a_dir).unwrap();
        assert!(take(&not_a_dir.join("agent.lock"), true).unwrap().is_none());
    }

    #[test]
    fn non_root_fails_when_the_lock_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("agent.lock");
        let err = take(&path, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn take_holds_the_lock_when_it_can() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.lock");
        let held = take(&path, true).unwrap();
        assert!(held.is_some());
        let other = File::open(&path).unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
    }
}
