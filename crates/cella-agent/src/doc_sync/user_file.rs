//! Opening a file in the remote user's home that the agent, usually root, reads and writes on that user's behalf.
//!
//! A path-based open as root follows whatever that user planted: a symlink at `~/.claude.json` pointing at the root-only proxy config would have root read it and sync it back into a file the user can read.
//! So every open here is relative to a directory descriptor and refuses a symlink, and the file and the directory holding it must belong to the anchor's owner.
//! The owner check is also what refuses a hard link to someone else's file, which a no-follow open cannot see.
//! A write creates and renames its temp file relative to the same checked descriptor, so a directory swapped after the check cannot redirect it.
//!
//! The anchor is the home directory itself, and it alone is opened following a symlink, and only when its parent (`/home`, or `/` for `/root`) is owned by root and writable by no group or other user, so the user can neither swap the home out from under it nor retarget a home that is a symlink.
//! The parent is checked through the descriptor the anchor is then opened relative to; in any other parent, such as a sticky world-writable `/tmp`, the anchor is opened without following a symlink.
//! Every component below it is opened without following one.
//! Directories between the anchor and the file's own directory are not owner-checked: `~/.claude` is a bind mount of the host's directory, owned by the host user's uid, which need not be the remote user's (a `root` remote user on a Linux host), and a mount point cannot be renamed away.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use nix::fcntl::{OFlag, open, openat, renameat};
use nix::sys::stat::Mode;
use nix::unistd::{UnlinkatFlags, unlinkat};

/// A file at a fixed relative path below an anchor directory.
#[derive(Debug, Clone)]
pub struct UserFile {
    anchor: PathBuf,
    dirs: Vec<OsString>,
    name: OsString,
    path: PathBuf,
}

/// What reading a [`UserFile`] found.
#[derive(Debug)]
pub enum Lookup {
    /// The file, or a directory on the way to it, does not exist.
    Absent,
    /// The file's contents.
    Found(Vec<u8>),
    /// Something on the way is a symlink, not a directory or regular file, or
    /// owned by someone other than the anchor's owner.
    Refused(io::Error),
}

/// Whether [`UserFile::write`] gave the file the owner it was asked for.
#[derive(Debug)]
pub enum Ownership {
    /// No owner was requested; the file belongs to the writer.
    Unrequested,
    /// The requested owner was set.
    Applied,
    /// The requested owner could not be set (a user-namespaced or rootless runtime, NFS `root_squash`), so the file belongs to the writer.
    Failed(io::Error),
}

impl UserFile {
    /// The file at `rel` below `anchor`, or `None` unless `rel` is one or more
    /// plain names (no `..`, `.`, or root).
    pub fn new(anchor: impl Into<PathBuf>, rel: impl AsRef<Path>) -> Option<Self> {
        let mut names = rel
            .as_ref()
            .components()
            .map(|component| match component {
                Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let name = names.pop()?;
        let anchor = anchor.into();
        let path = anchor.join(rel);
        Some(Self {
            anchor,
            dirs: names,
            name,
            path,
        })
    }

    /// The file at `path`, anchored at its parent directory.
    pub fn in_parent(path: &Path) -> Option<Self> {
        Self::new(path.parent()?, path.file_name()?)
    }

    /// The file's path, for watching, writing, and messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file through the checks in the module docs.
    pub fn read(&self) -> Lookup {
        lookup(self.read_expecting(None, TRUSTED_UID))
    }

    /// [`Self::read`] on a blocking thread.
    pub async fn read_async(&self) -> Lookup {
        let file = self.clone();
        tokio::task::spawn_blocking(move || file.read())
            .await
            .unwrap_or_else(|e| Lookup::Refused(io::Error::other(e)))
    }

    /// Replace the file with `contents` and `mode` through a temp file in its directory, reached through the checks [`Self::read`] makes.
    ///
    /// The agent daemon usually runs as root, and the fresh inode would then belong to root, which the remote user's `claude` cannot read.
    /// As root, the file is therefore handed to its directory's owner, taken from the checked descriptor and set on the temp file's handle before the rename.
    /// A non-root agent cannot give a file away, so it keeps its own ownership.
    ///
    /// # Errors
    ///
    /// Returns the error that refused a directory, or the I/O error from creating, filling or renaming the temp file.
    /// An owner that cannot be set is not an error but [`Ownership::Failed`].
    pub fn write(&self, contents: &[u8], mode: u32) -> io::Result<Ownership> {
        let hand_over = nix::unistd::geteuid().is_root();
        self.write_via(
            &cella_filesync::temp_name(&self.name),
            contents,
            mode,
            hand_over,
            TRUSTED_UID,
        )
    }

    /// [`Self::write`] through the temp name `tmp`, handing the file to the directory's owner when `hand_over` is set, with the anchor opened as [`open_anchor`] does with `trusted_uid`.
    fn write_via(
        &self,
        tmp: &OsStr,
        contents: &[u8],
        mode: u32,
        hand_over: bool,
        trusted_uid: u32,
    ) -> io::Result<Ownership> {
        let (dir, _) = self.open_dir(None, trusted_uid)?;
        let owner = if hand_over {
            let meta = dir.metadata()?;
            Some((meta.uid(), meta.gid()))
        } else {
            None
        };
        let mut file = create_temp_at(&dir, tmp, mode)?;
        let filled = fill(&mut file, contents, mode, owner);
        drop(file);
        let installed = filled.and_then(|ownership| {
            renameat(&dir, tmp, &dir, self.name.as_os_str())?;
            Ok(ownership)
        });
        if installed.is_err() {
            let _ = unlinkat(&dir, tmp, UnlinkatFlags::NoRemoveDir);
        }
        installed
    }

    /// Read the file, requiring `expected_uid` as the owner of the file and of
    /// its directory, or the anchor's owner when `None`, with the anchor
    /// opened as [`open_anchor`] does with `trusted_uid`.
    fn read_expecting(&self, expected_uid: Option<u32>, trusted_uid: u32) -> io::Result<Vec<u8>> {
        let (dir, uid) = self.open_dir(expected_uid, trusted_uid)?;
        // `O_NONBLOCK` keeps a FIFO planted at the name from blocking the open;
        // it has no effect on reading a regular file.
        let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        let mut file = File::from(openat(&dir, self.name.as_os_str(), flags, Mode::empty())?);
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(refusal(&self.path, "is not a regular file"));
        }
        check_owner(&self.path, &meta, uid)?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;
        Ok(contents)
    }

    /// Open the file's directory, walking from the anchor without following a
    /// symlink, and require it to be owned by `expected_uid` (the anchor's
    /// owner when `None`). Returns it with that uid.
    ///
    /// The anchor itself is opened as [`open_anchor`] does with `trusted_uid`.
    fn open_dir(&self, expected_uid: Option<u32>, trusted_uid: u32) -> io::Result<(File, u32)> {
        let flags = DIR_FLAGS | OFlag::O_NOFOLLOW;
        let mut dir = open_anchor(&self.anchor, trusted_uid)?;
        let uid = match expected_uid {
            Some(uid) => uid,
            None => dir.metadata()?.uid(),
        };
        for name in &self.dirs {
            dir = File::from(openat(&dir, name.as_os_str(), flags, Mode::empty())?);
        }
        if !self.dirs.is_empty() {
            let parent = self.path.parent().unwrap_or(&self.anchor);
            check_owner(parent, &dir.metadata()?, uid)?;
        }
        Ok((dir, uid))
    }
}

/// Flags for opening a directory on the way to a file, before `O_NOFOLLOW` is decided.
const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_CLOEXEC);

/// Flags for opening the anchor's parent, which is only checked and opened relative to.
///
/// On Linux it is opened with `O_PATH`, which needs no read permission on it, so a non-root agent still reaches a home under a `0711` `/home`.
#[cfg(target_os = "linux")]
const PARENT_FLAGS: OFlag = OFlag::O_PATH
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_CLOEXEC);
#[cfg(not(target_os = "linux"))]
const PARENT_FLAGS: OFlag = DIR_FLAGS;

/// The only owner of a directory whose entry for the anchor is followed when it is a symlink.
const TRUSTED_UID: u32 = 0;

/// Open the anchor directory, following it if it is a symlink only when its parent is owned by `trusted_uid` and writable by no group or other user.
///
/// The parent is opened and checked through one descriptor and the anchor opened relative to it, so the check and the open see the same directory.
/// An anchor with no parent or no final name is opened without following a symlink.
fn open_anchor(anchor: &Path, trusted_uid: u32) -> io::Result<File> {
    let nofollow = DIR_FLAGS | OFlag::O_NOFOLLOW;
    let (Some(parent), Some(name)) = (anchor.parent(), anchor.file_name()) else {
        return Ok(File::from(open(anchor, nofollow, Mode::empty())?));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = File::from(open(parent, PARENT_FLAGS, Mode::empty())?);
    let meta = parent.metadata()?;
    let trusted = meta.uid() == trusted_uid && meta.mode() & 0o022 == 0;
    let flags = if trusted { DIR_FLAGS } else { nofollow };
    Ok(File::from(openat(&parent, name, flags, Mode::empty())?))
}

/// Create `tmp` in `dir` exclusively with `mode`.
/// Whatever already sits at that name is unlinked once, without following it, and the create retried.
/// It mirrors `cella_filesync`'s `create_temp`, relative to a directory descriptor.
fn create_temp_at(dir: &File, tmp: &OsStr, mode: u32) -> io::Result<File> {
    let flags =
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mode = Mode::from_bits_truncate(mode);
    let create = || openat(dir, tmp, flags, mode).map(File::from);
    match create() {
        Err(nix::errno::Errno::EEXIST) => {
            unlinkat(dir, tmp, UnlinkatFlags::NoRemoveDir)?;
            Ok(create()?)
        }
        created => Ok(created?),
    }
}

/// Fill the temp file and set its mode, then its owner when one is given.
fn fill(
    file: &mut File,
    contents: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
) -> io::Result<Ownership> {
    cella_filesync::fill_temp(file, contents, mode)?;
    Ok(owner.map_or(Ownership::Unrequested, |(uid, gid)| {
        std::os::unix::fs::fchown(&*file, Some(uid), Some(gid))
            .map_or_else(Ownership::Failed, |()| Ownership::Applied)
    }))
}

/// Sort a read's outcome: a missing file is no document, anything else that
/// failed is a refusal.
fn lookup(read: io::Result<Vec<u8>>) -> Lookup {
    match read {
        Ok(contents) => Lookup::Found(contents),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Lookup::Absent,
        Err(e) => Lookup::Refused(e),
    }
}

fn check_owner(path: &Path, meta: &std::fs::Metadata, uid: u32) -> io::Result<()> {
    if meta.uid() == uid {
        Ok(())
    } else {
        Err(refusal(
            path,
            &format!("is owned by uid {}, not {uid}", meta.uid()),
        ))
    }
}

fn refusal(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {why}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn own_uid(dir: &Path) -> u32 {
        std::fs::metadata(dir).expect("tempdir metadata").uid()
    }

    #[test]
    fn reads_a_regular_file_owned_by_the_anchor_owner() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".claude/plugins")).expect("mkdir");
        std::fs::write(home.path().join(".claude/plugins/doc.json"), b"{}").expect("seed");
        let file = UserFile::new(home.path(), ".claude/plugins/doc.json").expect("plain names");

        let Lookup::Found(contents) = file.read() else {
            panic!("an owned regular file must be read");
        };
        assert_eq!(contents, b"{}");
    }

    #[test]
    fn a_missing_file_or_directory_is_absent() {
        let home = tempfile::tempdir().expect("tempdir");
        for rel in ["doc.json", "missing/doc.json"] {
            let file = UserFile::new(home.path(), rel).expect("plain names");
            assert!(matches!(file.read(), Lookup::Absent), "{rel}");
        }
    }

    /// The symlink's target is a file the anchor's owner owns, so only the
    /// no-follow open stands between root and reading it.
    #[test]
    fn a_symlink_at_the_file_is_not_read() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join("secret"), b"SECRET").expect("seed");
        std::os::unix::fs::symlink(home.path().join("secret"), home.path().join("doc.json"))
            .expect("symlink");
        let file = UserFile::new(home.path(), "doc.json").expect("plain names");

        let read = file.read();

        assert!(matches!(read, Lookup::Refused(_)), "{read:?}");
    }

    #[test]
    fn a_symlinked_directory_on_the_way_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join("elsewhere")).expect("mkdir");
        std::fs::write(home.path().join("elsewhere/doc.json"), b"SECRET").expect("seed");
        std::os::unix::fs::symlink(home.path().join("elsewhere"), home.path().join(".claude"))
            .expect("symlink");
        let file = UserFile::new(home.path(), ".claude/doc.json").expect("plain names");

        let read = file.read();

        assert!(matches!(read, Lookup::Refused(_)), "{read:?}");
    }

    /// The chain is checked when the write runs, not when the `UserFile` is built, and the write lands relative to the directory that check opened; a path-based write would follow the symlink swapped in afterwards.
    #[test]
    fn a_write_is_not_redirected_through_a_directory_swapped_for_a_symlink() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join(".claude")).expect("mkdir");
        std::fs::create_dir(home.path().join("elsewhere")).expect("mkdir");
        let file = UserFile::new(home.path(), ".claude/doc.json").expect("plain names");
        std::fs::remove_dir(home.path().join(".claude")).expect("rmdir");
        std::os::unix::fs::symlink(home.path().join("elsewhere"), home.path().join(".claude"))
            .expect("symlink");

        let written = file.write(b"{}", 0o600);

        assert!(written.is_err(), "{written:?}");
        let leftovers: Vec<_> = std::fs::read_dir(home.path().join("elsewhere"))
            .expect("read_dir")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_write_replaces_the_file_with_the_given_mode_and_leaves_no_temp() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join(".claude")).expect("mkdir");
        std::fs::write(home.path().join(".claude/doc.json"), b"old").expect("seed");
        let file = UserFile::new(home.path(), ".claude/doc.json").expect("plain names");

        let ownership = file.write(b"new", 0o600).expect("owned chain");

        assert!(matches!(ownership, Ownership::Unrequested), "{ownership:?}");
        assert_eq!(std::fs::read(file.path()).expect("written"), b"new");
        let mode = std::fs::metadata(file.path()).expect("metadata").mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        let names: Vec<_> = std::fs::read_dir(home.path().join(".claude"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["doc.json"]);
    }

    #[test]
    fn a_write_replaces_a_symlink_at_the_file_instead_of_following_it() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join("victim"), b"untouched").expect("seed");
        std::os::unix::fs::symlink(home.path().join("victim"), home.path().join("doc.json"))
            .expect("symlink");
        let file = UserFile::new(home.path(), "doc.json").expect("plain names");

        file.write(b"new", 0o600).expect("owned chain");

        assert_eq!(
            std::fs::read(home.path().join("victim")).expect("victim"),
            b"untouched"
        );
        let meta = std::fs::symlink_metadata(file.path()).expect("metadata");
        assert!(meta.is_file());
        assert_eq!(std::fs::read(file.path()).expect("written"), b"new");
    }

    /// A killed writer whose pid comes back after a container restart leaves something at the very temp name the next write picks.
    #[test]
    fn a_write_replaces_a_symlink_left_at_the_temp_name_without_following_it() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join("victim"), b"untouched").expect("seed");
        let tmp = OsStr::new(".doc.json.cella-tmp.planted");
        std::os::unix::fs::symlink(home.path().join("victim"), home.path().join(tmp))
            .expect("symlink");
        let file = UserFile::new(home.path(), "doc.json").expect("plain names");

        file.write_via(tmp, b"new", 0o600, false, TRUSTED_UID)
            .expect("owned chain");

        assert_eq!(
            std::fs::read(home.path().join("victim")).expect("victim"),
            b"untouched"
        );
        assert_eq!(std::fs::read(file.path()).expect("written"), b"new");
        assert!(std::fs::symlink_metadata(home.path().join(tmp)).is_err());
    }

    /// A non-root test can only "hand over" a file to itself, so what this shows is that the owner is requested, and taken from the directory.
    #[test]
    fn a_handed_over_write_requests_the_directorys_owner() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join(".claude")).expect("mkdir");
        let file = UserFile::new(home.path(), ".claude/doc.json").expect("plain names");
        let dir = std::fs::metadata(home.path().join(".claude")).expect("metadata");

        let ownership = file
            .write_via(OsStr::new(".doc.json.tmp"), b"{}", 0o600, true, TRUSTED_UID)
            .expect("owned chain");

        assert!(matches!(ownership, Ownership::Applied), "{ownership:?}");
        let meta = std::fs::metadata(file.path()).expect("metadata");
        assert_eq!((meta.uid(), meta.gid()), (dir.uid(), dir.gid()));
    }

    /// A parent directory with `mode` holding `real/.claude/doc.json` (containing `old`) and a `home` symlink to `real`, and the file reached through that symlink.
    fn symlinked_home(mode: u32) -> (tempfile::TempDir, UserFile) {
        let parent = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(parent.path().join("real/.claude")).expect("mkdir");
        std::fs::write(parent.path().join("real/.claude/doc.json"), b"old").expect("seed");
        std::os::unix::fs::symlink(parent.path().join("real"), parent.path().join("home"))
            .expect("symlink");
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(mode))
            .expect("chmod");
        let file =
            UserFile::new(parent.path().join("home"), ".claude/doc.json").expect("plain names");
        (parent, file)
    }

    /// With the parent owned by the trusted uid and writable by no one else, a home that is itself a symlink (to a home on another volume, say) is one the user cannot retarget.
    #[test]
    fn a_symlinked_home_in_a_trusted_parent_is_read_and_written() {
        let (parent, file) = symlinked_home(0o755);
        let trusted = own_uid(parent.path());

        let contents = file
            .read_expecting(None, trusted)
            .expect("a home reached through a symlink in a trusted parent must be read");
        assert_eq!(contents, b"old");
        file.write_via(OsStr::new(".doc.json.tmp"), b"new", 0o600, false, trusted)
            .expect("a home reached through a symlink in a trusted parent must be written");
        assert_eq!(
            std::fs::read(parent.path().join("real/.claude/doc.json")).expect("written"),
            b"new"
        );
    }

    /// A `/home` of mode `0711` or `0751` lets a non-root agent pass through it to its home but not list it.
    /// The test drops its own read permission on a parent it owns, since root would read the parent regardless.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_home_in_a_parent_the_agent_cannot_read_is_read() {
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: root reads the parent whatever its mode");
            return;
        }
        let parent = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(parent.path().join("home/.claude")).expect("mkdir");
        std::fs::write(parent.path().join("home/.claude/doc.json"), b"{}").expect("seed");
        let file =
            UserFile::new(parent.path().join("home"), ".claude/doc.json").expect("plain names");
        let trusted = own_uid(parent.path());
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o311))
            .expect("chmod");

        let read = file.read_expecting(None, trusted);

        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod back");
        assert_eq!(
            read.expect("a home under an unreadable parent must be read"),
            b"{}"
        );
    }

    /// Run as a user other than root, the test owns the parent, which in production would let that user retarget the home at a directory of another user's.
    #[test]
    fn a_symlinked_home_in_a_parent_not_owned_by_root_is_refused() {
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: run as root, the parent is owned by the trusted uid");
            return;
        }
        let (parent, file) = symlinked_home(0o755);

        let read = file.read();
        let written = file.write(b"new", 0o600);

        assert!(matches!(read, Lookup::Refused(_)), "{read:?}");
        assert!(written.is_err(), "{written:?}");
        assert_eq!(
            std::fs::read(parent.path().join("real/.claude/doc.json")).expect("untouched"),
            b"old"
        );
    }

    /// Anyone who can write the parent can retarget the home, whoever owns it; a sticky `/tmp` is one such parent.
    #[test]
    fn a_symlinked_home_in_a_group_or_other_writable_parent_is_refused() {
        for mode in [0o775, 0o757, 0o1777] {
            let (parent, file) = symlinked_home(mode);
            let trusted = own_uid(parent.path());

            let read = lookup(file.read_expecting(None, trusted));

            assert!(matches!(read, Lookup::Refused(_)), "{mode:o}: {read:?}");
        }
    }

    /// A non-root test cannot chown a file away, so the expected owner is
    /// what differs instead. The file sits directly in the anchor, which is
    /// never owner-checked, so the file's own check is the only one in play.
    #[test]
    fn a_file_owned_by_someone_else_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join("doc.json"), b"{}").expect("seed");
        let file = UserFile::new(home.path(), "doc.json").expect("plain names");
        let uid = own_uid(home.path());

        assert!(file.read_expecting(Some(uid), TRUSTED_UID).is_ok());
        let err = file
            .read_expecting(Some(uid.wrapping_add(1)), TRUSTED_UID)
            .expect_err("a foreign owner must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_directory_owned_by_someone_else_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join(".claude")).expect("mkdir");
        std::fs::write(home.path().join(".claude/doc.json"), b"{}").expect("seed");
        let file = UserFile::new(home.path(), ".claude/doc.json").expect("plain names");
        let uid = own_uid(home.path());

        let err = file
            .open_dir(Some(uid.wrapping_add(1)), TRUSTED_UID)
            .expect_err("a foreign directory must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    /// Without a non-blocking open, a FIFO at the name would hang the read
    /// until some writer opened it.
    #[test]
    fn a_fifo_at_the_file_is_refused_without_blocking() {
        let home = tempfile::tempdir().expect("tempdir");
        nix::unistd::mkfifo(
            &home.path().join("doc.json"),
            Mode::from_bits_truncate(0o600),
        )
        .expect("mkfifo");
        let file = UserFile::new(home.path(), "doc.json").expect("plain names");

        let read = file.read();

        assert!(matches!(read, Lookup::Refused(_)), "{read:?}");
    }

    #[test]
    fn only_plain_names_below_the_anchor_are_accepted() {
        assert!(UserFile::new("/home/vscode", "../root/.claude.json").is_none());
        assert!(UserFile::new("/home/vscode", "/etc/shadow").is_none());
        assert!(UserFile::new("/home/vscode", "").is_none());
        let file = UserFile::new("/home/vscode", ".claude/plugins/installed_plugins.json")
            .expect("plain names");
        assert_eq!(
            file.path(),
            Path::new("/home/vscode/.claude/plugins/installed_plugins.json")
        );
        let file = UserFile::in_parent(Path::new("/home/vscode/.claude.json")).expect("a file");
        assert_eq!(file.anchor, Path::new("/home/vscode"));
        assert!(file.dirs.is_empty());
    }
}
