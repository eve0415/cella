//! OCI layer extraction utilities.
//!
//! All extraction goes through [`extract_layer`], which calls the safe
//! [`unpack_archive`] helper.  That helper rejects any archive entry whose
//! path or link target would write outside the destination tree.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use oci_distribution::manifest::{
    IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use thiserror::Error;
use tracing::warn;

use crate::limits::{LimitedReader, MAX_BLOB_DECOMPRESSED_BYTES, limit_from_io_error};

/// Media type for devcontainer feature layers (plain tar).
pub const DEVCONTAINERS_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while extracting an OCI layer tarball.
#[derive(Debug, Error)]
pub enum ExtractionError {
    /// An entry's path contains `..`, an absolute root, or a Windows drive
    /// prefix — it would write outside the destination tree.
    #[error("path traversal in tar entry '{path}': {reason}")]
    PathTraversal { path: String, reason: &'static str },

    /// A symlink or hard-link entry's target would resolve outside the
    /// destination tree (absolute target or one whose components escape `dest`).
    #[error("unsafe link target in tar entry '{entry}': target '{target}' escapes destination")]
    UnsafeLinkTarget { entry: String, target: String },

    /// An entry that `Entry::unpack_in` silently skipped — meaning the tar
    /// crate also considers it unsafe.  We surface it as a hard error.
    #[error("tar entry '{path}' was skipped as unsafe by the tar crate")]
    EntrySkipped { path: String },

    /// Decompressed output exceeded the configured cap — possible zip-bomb.
    #[error("decompressed output exceeds limit of {limit} bytes")]
    DecompressedTooLarge { limit: u64 },

    /// Underlying I/O or tar error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Returns `true` when the media type indicates a layer we can extract.
pub fn is_extractable_layer(media_type: &str) -> bool {
    matches!(
        media_type,
        IMAGE_LAYER_GZIP_MEDIA_TYPE
            | IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE
            | IMAGE_LAYER_MEDIA_TYPE
            | DEVCONTAINERS_LAYER_MEDIA_TYPE
    ) || media_type.contains("tar+gzip")
        || media_type.contains("tar.gzip")
}

/// Extract a layer blob (gzip tarball or plain tar) into `dest`.
///
/// Sniffs the magic bytes to determine encoding regardless of `media_type`
/// (the registry sometimes lies).  All entries are validated by
/// [`unpack_archive`] before any file is written.  Decompressed output is
/// capped at [`MAX_BLOB_DECOMPRESSED_BYTES`]; exceeding it returns
/// [`ExtractionError::DecompressedTooLarge`].
///
/// # Errors
///
/// Returns [`ExtractionError`] if the archive contains a path-traversal or
/// unsafe link entry, if an I/O error occurs, or if the decompressed size
/// exceeds the cap.
pub fn extract_layer(blob: &[u8], media_type: &str, dest: &Path) -> Result<(), ExtractionError> {
    extract_layer_with_limit(blob, media_type, dest, MAX_BLOB_DECOMPRESSED_BYTES)
}

/// Like [`extract_layer`] but with a caller-supplied decompression cap.
///
/// Intended for tests that need a small cap to exercise the limit path without
/// allocating gigabytes.
///
/// # Errors
///
/// Same as [`extract_layer`].
pub fn extract_layer_with_limit(
    blob: &[u8],
    media_type: &str,
    dest: &Path,
    decompress_limit: u64,
) -> Result<(), ExtractionError> {
    let is_gzip = blob.len() >= 2 && blob[0] == 0x1f && blob[1] == 0x8b;

    if media_type.contains("gzip") || media_type == IMAGE_LAYER_GZIP_MEDIA_TYPE {
        if is_gzip {
            let limited = LimitedReader::new(GzDecoder::new(blob), decompress_limit);
            unpack_archive(tar::Archive::new(limited), dest).map_err(map_limit_error)
        } else {
            warn!("layer declared as gzip but does not have gzip magic; trying raw tar");
            unpack_archive(tar::Archive::new(blob), dest)
        }
    } else if is_gzip {
        warn!("layer declared as plain tar but has gzip magic; decompressing");
        let limited = LimitedReader::new(GzDecoder::new(blob), decompress_limit);
        unpack_archive(tar::Archive::new(limited), dest).map_err(map_limit_error)
    } else {
        unpack_archive(tar::Archive::new(blob), dest)
    }
}

/// Remap an [`ExtractionError::Io`] that originated from the decompression
/// cap into the typed [`ExtractionError::DecompressedTooLarge`] variant.
///
/// Non-limit errors pass through unchanged.
fn map_limit_error(err: ExtractionError) -> ExtractionError {
    if let ExtractionError::Io(io_err) = &err
        && let Some(limit) = limit_from_io_error(io_err)
    {
        return ExtractionError::DecompressedTooLarge { limit };
    }
    err
}

// ---------------------------------------------------------------------------
// Core safe-extraction helper
// ---------------------------------------------------------------------------

/// Iterate every entry in `archive` and extract it under `dest`, applying
/// strict path-traversal and link-target validation before writing anything.
///
/// Validation rules (applied to every entry):
/// 1. The entry path must not contain `..`, an absolute root, or a Windows
///    drive prefix.
/// 2. For symlink and hard-link entries: the link target must not be absolute
///    and must not resolve to a path outside `dest` when joined with the
///    entry's parent directory.
/// 3. Any entry that `Entry::unpack_in` silently skips (`Ok(false)`) is
///    surfaced as [`ExtractionError::EntrySkipped`] rather than silently
///    ignored.
fn unpack_archive<R: Read>(
    mut archive: tar::Archive<R>,
    dest: &Path,
) -> Result<(), ExtractionError> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        validate_entry(&entry, dest)?;
        let path = entry_path_string(&entry);
        let unpacked = entry.unpack_in(dest)?;
        if !unpacked {
            return Err(ExtractionError::EntrySkipped { path });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Return the entry path as a lossless display string (for error messages).
fn entry_path_string<R: Read>(entry: &tar::Entry<'_, R>) -> String {
    entry
        .path()
        .map_or_else(|_| "<invalid path>".to_owned(), |p| p.display().to_string())
}

/// Validate a single archive entry for path traversal and unsafe links.
fn validate_entry<R: Read>(entry: &tar::Entry<'_, R>, dest: &Path) -> Result<(), ExtractionError> {
    validate_entry_path(entry)?;
    validate_link_target(entry, dest)?;
    Ok(())
}

/// Reject paths that contain `..`, absolute roots, or drive prefixes.
fn validate_entry_path<R: Read>(entry: &tar::Entry<'_, R>) -> Result<(), ExtractionError> {
    let path = entry.path().map_err(ExtractionError::Io)?;
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(ExtractionError::PathTraversal {
                    path: path.display().to_string(),
                    reason: "contains '..' component",
                });
            }
            Component::RootDir => {
                return Err(ExtractionError::PathTraversal {
                    path: path.display().to_string(),
                    reason: "absolute path (root directory component)",
                });
            }
            Component::Prefix(_) => {
                return Err(ExtractionError::PathTraversal {
                    path: path.display().to_string(),
                    reason: "Windows drive prefix",
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Reject symlink/hardlink entries whose target escapes `dest`.
///
/// - Absolute targets are rejected outright.
/// - Relative targets are resolved against the entry's parent directory
///   (inside `dest`) and checked for containment.
fn validate_link_target<R: Read>(
    entry: &tar::Entry<'_, R>,
    dest: &Path,
) -> Result<(), ExtractionError> {
    let header = entry.header();
    let kind = header.entry_type();
    if !kind.is_symlink() && !kind.is_hard_link() {
        return Ok(());
    }

    let link_target = match entry.link_name().map_err(ExtractionError::Io)? {
        Some(t) => t.into_owned(),
        None => return Ok(()),
    };

    let entry_path = entry.path().map_err(ExtractionError::Io)?;
    let entry_name = entry_path.display().to_string();
    let target_display = link_target.display().to_string();

    // Absolute link targets always escape the destination.
    if link_target.is_absolute() {
        return Err(ExtractionError::UnsafeLinkTarget {
            entry: entry_name,
            target: target_display,
        });
    }

    // Resolve the relative target against its correct base inside dest.
    // POSIX semantics differ by link kind: symlink targets are relative to the
    // link's own directory, while hardlink targets are relative to the archive
    // root (dest). Using the entry parent for both would under-validate
    // hardlinks (e.g. `a/b/link -> ../../etc/passwd` would look contained).
    let base = if kind.is_hard_link() {
        dest.to_path_buf()
    } else {
        entry_path
            .parent()
            .map_or_else(|| dest.to_path_buf(), |p| dest.join(p))
    };
    if escapes_dest(&base.join(&link_target), dest) {
        return Err(ExtractionError::UnsafeLinkTarget {
            entry: entry_name,
            target: target_display,
        });
    }

    Ok(())
}

/// Returns `true` when the normalised form of `candidate` escapes `dest`.
///
/// We walk the path components and track depth; a `..` that would pop above
/// zero depth means escape.  This is intentionally conservative and does not
/// require the path to exist on disk — it works for not-yet-created paths.
fn escapes_dest(candidate: &Path, dest: &Path) -> bool {
    // Strip the dest prefix; if that fails the path already escapes.
    let Ok(relative) = candidate.strip_prefix(dest) else {
        return !is_contained_by_normalization(candidate, dest);
    };
    has_escape_components(relative)
}

/// Fallback containment check using component-level normalisation (no I/O).
fn is_contained_by_normalization(candidate: &Path, dest: &Path) -> bool {
    let norm_candidate = normalize_path(candidate);
    let norm_dest = normalize_path(dest);
    norm_candidate.starts_with(norm_dest)
}

/// Walk components and count depth; returns `true` if any `..` would pop
/// above the root of the path segment.
fn has_escape_components(path: &Path) -> bool {
    let mut depth: i64 = 0;
    for component in path.components() {
        match component {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    false
}

/// Normalise a path by resolving `.` and `..` without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use oci_distribution::manifest::IMAGE_LAYER_MEDIA_TYPE;
    use tempfile::TempDir;

    use super::*;

    // -----------------------------------------------------------------------
    // Helpers to build in-memory tarballs
    //
    // The `tar` crate's Builder validates paths on write (rejects `..` and
    // absolute paths), so malicious entries must be constructed at the raw
    // byte level.  We write a minimal POSIX ustar header block (512 bytes)
    // followed by the data blocks and a two-block end-of-archive marker.
    // -----------------------------------------------------------------------

    /// Write a zero-padded ASCII field into a fixed-length byte slice.
    fn write_field(buf: &mut [u8], s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&bytes[..len]);
    }

    /// Compute the standard tar header checksum (sum of all bytes with the
    /// checksum field treated as eight ASCII spaces).
    fn tar_checksum(header: &[u8; 512]) -> u32 {
        let mut sum = 0u32;
        for (i, &b) in header.iter().enumerate() {
            // checksum field is bytes 148..156; treat as spaces during computation
            if (148..156).contains(&i) {
                sum += u32::from(b' ');
            } else {
                sum += u32::from(b);
            }
        }
        sum
    }

    /// Build a raw 512-byte ustar header block.
    ///
    /// `typeflag`: `b'0'` = regular file, `b'2'` = symlink.
    fn raw_header(path: &str, typeflag: u8, size: u64, linkname: &str) -> [u8; 512] {
        let mut h = [0u8; 512];
        write_field(&mut h[0..100], path); // name
        write_field(&mut h[100..108], "0000644\0"); // mode
        write_field(&mut h[108..116], "0001750\0"); // uid
        write_field(&mut h[116..124], "0001750\0"); // gid
        // size in octal, 11 digits + NUL
        let size_str = format!("{size:011o}\0");
        write_field(&mut h[124..136], &size_str);
        write_field(&mut h[136..148], "00000000000\0"); // mtime
        // checksum placeholder — will be filled below
        write_field(&mut h[148..156], "        "); // 8 spaces
        h[156] = typeflag;
        write_field(&mut h[157..257], linkname); // linkname
        write_field(&mut h[257..265], "ustar  \0"); // GNU magic (8 bytes: "ustar", two spaces, NUL)
        let cksum = tar_checksum(&h);
        let cksum_str = format!("{cksum:06o}\0 ");
        write_field(&mut h[148..156], &cksum_str);
        h
    }

    /// Build a plain tar from raw entries, bypassing the tar crate's path
    /// validation so we can craft malicious paths for security tests.
    ///
    /// `entries`: `(path, typeflag, linkname, data)`
    fn build_raw_tar(entries: &[(&str, u8, &str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, typeflag, linkname, data) in entries {
            let header = raw_header(path, *typeflag, data.len() as u64, linkname);
            out.extend_from_slice(&header);
            out.extend_from_slice(data);
            // Pad to next 512-byte boundary
            let remainder = data.len() % 512;
            if remainder != 0 {
                out.extend(std::iter::repeat_n(0u8, 512 - remainder));
            }
        }
        // End-of-archive: two zero-filled 512-byte blocks
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    /// Build a benign tar using the safe tar Builder (paths are validated).
    fn build_safe_tar(entries: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let buf = Vec::new();
        let mut builder = tar::Builder::new(buf);
        for (path, link_target, content) in entries {
            let mut header = tar::Header::new_gnu();
            if let Some(target) = link_target {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_link(&mut header, path, target)
                    .expect("append_link");
            } else {
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, *content)
                    .expect("append_data");
            }
        }
        builder.into_inner().expect("tar finish")
    }

    fn gz_compress(data: &[u8]) -> Vec<u8> {
        let buf = Vec::new();
        let mut enc = GzEncoder::new(buf, Compression::fast());
        enc.write_all(data).expect("gz write");
        enc.finish().expect("gz finish")
    }

    fn dest_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // -----------------------------------------------------------------------
    // Path-traversal: plain tar
    //
    // Malicious paths are built with `build_raw_tar` to bypass the tar
    // crate's own write-time path validation.
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_dotdot_entry_plain() {
        // Raw tar with "../evil.txt" — tar Builder would refuse to write this.
        let tar = build_raw_tar(&[("../evil.txt", b'0', "", b"pwned")]);
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(err, ExtractionError::PathTraversal { .. }),
            "expected PathTraversal, got {err}"
        );
        // Nothing must have been written outside dest
        assert!(
            !dest.path().parent().unwrap().join("evil.txt").exists(),
            "evil.txt must not be written outside dest"
        );
    }

    #[test]
    fn rejects_absolute_path_entry_plain() {
        // Raw tar with "/tmp/evil.txt".  The tar crate strips the leading '/'
        // and returns Ok(false); we surface that as an error.
        let tar = build_raw_tar(&[("/tmp/evil.txt", b'0', "", b"pwned")]);
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(
                err,
                ExtractionError::PathTraversal { .. } | ExtractionError::EntrySkipped { .. }
            ),
            "expected path-escape error, got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Path-traversal: gz tar
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_dotdot_entry_gz() {
        let tar = gz_compress(&build_raw_tar(&[("../evil.txt", b'0', "", b"pwned")]));
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_GZIP_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(err, ExtractionError::PathTraversal { .. }),
            "expected PathTraversal, got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Symlink target escaping
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_symlink_with_dotdot_target() {
        // symlink entry "link" -> "../../outside"
        // typeflag b'2' = symlink; linkname field carries the target.
        let tar = build_raw_tar(&[("link", b'2', "../../outside", b"")]);
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(err, ExtractionError::UnsafeLinkTarget { .. }),
            "expected UnsafeLinkTarget, got {err}"
        );
    }

    #[test]
    fn rejects_symlink_with_absolute_target() {
        let tar = build_raw_tar(&[("link", b'2', "/etc/passwd", b"")]);
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(err, ExtractionError::UnsafeLinkTarget { .. }),
            "expected UnsafeLinkTarget, got {err}"
        );
    }

    #[test]
    fn rejects_symlink_with_absolute_target_gz() {
        let tar = gz_compress(&build_raw_tar(&[("link", b'2', "/etc/passwd", b"")]));
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_GZIP_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(err, ExtractionError::UnsafeLinkTarget { .. }),
            "expected UnsafeLinkTarget, got {err}"
        );
    }

    #[test]
    fn rejects_hardlink_escaping_via_archive_root() {
        // Hardlink targets are relative to the archive root, not the entry's
        // own directory. `a/b/link -> ../../etc/passwd` resolves to
        // `dest/../../etc/passwd` and must be rejected by first-line
        // validation (typeflag b'1' = hard link).
        let tar = build_raw_tar(&[("a/b/link", b'1', "../../etc/passwd", b"")]);
        let dest = dest_dir();
        let err = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap_err();
        assert!(
            matches!(err, ExtractionError::UnsafeLinkTarget { .. }),
            "expected UnsafeLinkTarget, got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Benign archive succeeds (uses safe Builder — paths are valid)
    // -----------------------------------------------------------------------

    #[test]
    fn benign_plain_tar_succeeds() {
        let tar = build_safe_tar(&[
            ("subdir/hello.txt", None, b"world"),
            ("readme.txt", None, b"ok"),
        ]);
        let dest = dest_dir();
        extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("subdir/hello.txt")).unwrap(),
            "world"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("readme.txt")).unwrap(),
            "ok"
        );
    }

    #[test]
    fn benign_gz_tar_succeeds() {
        let tar = gz_compress(&build_safe_tar(&[("hello.txt", None, b"world")]));
        let dest = dest_dir();
        extract_layer(&tar, IMAGE_LAYER_GZIP_MEDIA_TYPE, dest.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("hello.txt")).unwrap(),
            "world"
        );
    }

    // -----------------------------------------------------------------------
    // safe-relative symlink inside dest is accepted
    // -----------------------------------------------------------------------

    #[test]
    fn safe_relative_symlink_accepted() {
        // symlink "link" -> "target.txt" (both in dest root — safe)
        let tar = build_safe_tar(&[
            ("target.txt", None, b"data"),
            ("link", Some("target.txt"), b""),
        ]);
        let dest = dest_dir();
        // May error on I/O if the OS denies symlink creation; that's fine —
        // we just assert it does NOT produce a path-traversal error.
        let result = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path());
        assert!(
            result.is_ok() || matches!(result, Err(ExtractionError::Io(_))),
            "should not be a traversal error; got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // `./`-prefixed entries (ubiquitous in real OCI tarballs) are accepted
    // -----------------------------------------------------------------------

    #[test]
    fn dotslash_prefixed_entries_accepted() {
        let tar = build_raw_tar(&[("./.devcontainer/devcontainer.json", b'0', "", b"{}")]);
        let dest = dest_dir();
        extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join(".devcontainer/devcontainer.json")).unwrap(),
            "{}"
        );
    }

    // -----------------------------------------------------------------------
    // Writing through an accepted intra-dest symlink cannot escape dest
    // -----------------------------------------------------------------------

    #[test]
    fn write_through_intra_dest_symlink_stays_inside_dest() {
        // Entry 1: dir "subdir/"; entry 2: symlink "subdir/link" -> ".."
        // (resolves to dest — contained); entry 3: file "subdir/link/evil.txt"
        // whose components are all Normal. If extraction follows the symlink,
        // the file must land inside dest, never outside it.
        let tar = build_raw_tar(&[
            ("subdir/", b'5', "", b""),
            ("subdir/link", b'2', "..", b""),
            ("subdir/link/evil.txt", b'0', "", b"pwned"),
        ]);
        let dest = dest_dir();
        let result = extract_layer(&tar, IMAGE_LAYER_MEDIA_TYPE, dest.path());
        // Accepted or rejected are both fine — escaping dest is not.
        drop(result);
        assert!(
            !dest.path().parent().unwrap().join("evil.txt").exists(),
            "evil.txt must not be written outside dest"
        );
    }

    // -----------------------------------------------------------------------
    // is_extractable_layer
    // -----------------------------------------------------------------------

    #[test]
    fn is_extractable_layer_recognises_oci_gzip() {
        assert!(is_extractable_layer(IMAGE_LAYER_GZIP_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_recognises_docker_gzip() {
        assert!(is_extractable_layer(IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_recognises_plain_tar() {
        assert!(is_extractable_layer(IMAGE_LAYER_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_recognises_devcontainers_tar() {
        assert!(is_extractable_layer(DEVCONTAINERS_LAYER_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_rejects_manifest_type() {
        assert!(!is_extractable_layer(
            "application/vnd.oci.image.manifest.v1+json"
        ));
    }

    #[test]
    fn is_extractable_layer_rejects_config() {
        assert!(!is_extractable_layer(
            "application/vnd.oci.image.config.v1+json"
        ));
    }

    // -----------------------------------------------------------------------
    // Decompression size caps
    // -----------------------------------------------------------------------

    /// Build a gzip-compressed tar whose decompressed size exceeds a small cap.
    ///
    /// The content is highly compressible (repeated zeros) to simulate a
    /// zip-bomb: tiny compressed, large decompressed.
    fn build_gz_tar_with_large_content(decompressed_content_size: usize) -> Vec<u8> {
        let content = vec![0u8; decompressed_content_size];
        gz_compress(&build_safe_tar(&[("big.bin", None, &content)]))
    }

    #[test]
    fn decompression_cap_blocks_zip_bomb() {
        // 2048 bytes decompressed content; cap is 1024 bytes → must error.
        let gz_tar = build_gz_tar_with_large_content(2048);
        let dest = dest_dir();
        let result =
            extract_layer_with_limit(&gz_tar, IMAGE_LAYER_GZIP_MEDIA_TYPE, dest.path(), 1024);
        assert!(
            result.is_err(),
            "expected error for oversized decompressed output, got Ok"
        );
    }

    #[test]
    fn decompression_cap_surfaces_typed_variant() {
        // Regression: exceeding the decompression cap must surface the typed
        // `DecompressedTooLarge` variant (carrying the limit), not an opaque
        // `Io` error — callers and the docs rely on this distinction.
        let gz_tar = build_gz_tar_with_large_content(4096);
        let dest = dest_dir();
        let err = extract_layer_with_limit(&gz_tar, IMAGE_LAYER_GZIP_MEDIA_TYPE, dest.path(), 1024)
            .unwrap_err();
        match err {
            ExtractionError::DecompressedTooLarge { limit } => {
                assert_eq!(limit, 1024, "the configured cap must be reported");
            }
            other => panic!("expected DecompressedTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn decompression_cap_allows_normal_archive() {
        // Small archive well under any reasonable cap.
        let gz_tar = build_gz_tar_with_large_content(512);
        let dest = dest_dir();
        extract_layer_with_limit(&gz_tar, IMAGE_LAYER_GZIP_MEDIA_TYPE, dest.path(), 64 * 1024)
            .expect("small archive should succeed");
        assert!(
            dest.path().join("big.bin").exists(),
            "big.bin should have been extracted"
        );
    }
}
