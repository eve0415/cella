//! `features package` — pack local feature source trees into distributable tarballs.
//!
//! Implements the same contract as `devcontainer features package`:
//! - Single-feature mode: `<target>/devcontainer-feature.json` exists at root.
//! - Collection mode: `<target>` is a directory of `<id>/` subdirs, each with its own
//!   `devcontainer-feature.json`.
//!
//! Each feature is packed into `devcontainer-feature-<id>.tgz` (all paths relative to `.`).
//! A `devcontainer-collection.json` is written to the output directory.

use std::fs;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use serde_json::Value;

use crate::error::FeatureError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Options forwarded from the CLI.
#[derive(Debug, Clone)]
pub struct PackageOptions {
    /// Path to the feature source tree (single feature dir or collection root).
    pub target: PathBuf,
    /// Directory where tarballs and `devcontainer-collection.json` are written.
    pub output_folder: PathBuf,
    /// Delete the output folder before packaging if it already exists.
    pub force_clean_output_folder: bool,
}

/// A packaged feature entry — the data that lands in `devcontainer-collection.json`.
///
/// Typed accessors are provided for the well-known fields; the full metadata map
/// (`raw`) is used when serializing to collection JSON to avoid duplicate keys.
#[derive(Debug, Clone)]
pub struct PackagedFeature {
    pub id: String,
    pub version: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Full raw metadata from `devcontainer-feature.json` (including `id`, `version`, etc.).
    pub raw: serde_json::Map<String, Value>,
}

/// Written to `<output>/devcontainer-collection.json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionJson {
    pub source_information: SourceInformation,
    pub features: Vec<Value>,
}

/// `sourceInformation` block inside `devcontainer-collection.json`.
#[derive(Debug, Serialize)]
pub struct SourceInformation {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

/// Summary returned after a successful packaging run.
#[derive(Debug)]
pub struct PackageResult {
    pub output_folder: PathBuf,
    pub features: Vec<PackagedFeature>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Package one or more features from `opts.target` into `opts.output_folder`.
///
/// # Errors
///
/// - Output folder already exists and `force_clean_output_folder` is false.
/// - `devcontainer-feature.json` missing (single mode) or `install.sh` missing (collection mode).
/// - Required fields (`id`, `version`, `name`) missing in metadata.
/// - Any I/O error.
pub fn package(opts: &PackageOptions) -> Result<PackageResult, PackageError> {
    let target = &opts.target;
    let output = &opts.output_folder;

    tracing::debug!(target = %target.display(), output = %output.display(), "starting feature packaging");

    // Guard: output folder must not already exist (or force-clean it).
    if output.exists() {
        if opts.force_clean_output_folder {
            tracing::debug!(output = %output.display(), "force-cleaning existing output folder");
            fs::remove_dir_all(output).map_err(PackageError::Io)?;
        } else {
            return Err(PackageError::OutputFolderExists {
                path: output.clone(),
            });
        }
    }

    fs::create_dir_all(output).map_err(PackageError::Io)?;

    let single_manifest = target.join("devcontainer-feature.json");
    if single_manifest.is_file() {
        tracing::debug!("detected single-feature mode");
        package_single(target, output)
    } else {
        tracing::debug!("detected collection mode");
        package_collection(target, output)
    }
}

// ---------------------------------------------------------------------------
// Single-feature mode
// ---------------------------------------------------------------------------

fn package_single(target: &Path, output: &Path) -> Result<PackageResult, PackageError> {
    // The official CLI does NOT require `install.sh` in single-feature mode —
    // only collection mode enforces it (see `package_collection`). We mirror
    // that behavior intentionally to stay drop-in compatible.
    let manifest_path = target.join("devcontainer-feature.json");
    let mut meta = read_manifest(&manifest_path)?;

    let packaged = PackagedFeature::from_meta(&mut meta, None)?;

    tracing::debug!(
        id = packaged.id,
        version = packaged.version,
        "packaging single feature"
    );

    let archive_path = output.join(archive_name(&packaged.id));
    write_tgz(target, &archive_path, &packaged.raw)?;

    tracing::debug!(archive = %archive_path.display(), "wrote feature archive");

    write_collection_json(output, std::slice::from_ref(&packaged))?;

    Ok(PackageResult {
        output_folder: output.to_owned(),
        features: vec![packaged],
    })
}

// ---------------------------------------------------------------------------
// Collection mode
// ---------------------------------------------------------------------------

fn package_collection(target: &Path, output: &Path) -> Result<PackageResult, PackageError> {
    let entries = fs::read_dir(target).map_err(PackageError::Io)?;

    let mut feature_dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
        .map(|e| e.path())
        .collect();

    // Stable iteration order.
    feature_dirs.sort();

    let mut packaged_features: Vec<PackagedFeature> = Vec::new();

    for dir in &feature_dirs {
        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();

        // Skip hidden directories.
        if dir_name.starts_with('.') {
            tracing::trace!(dir = dir_name, "skipping hidden directory");
            continue;
        }

        let manifest_path = dir.join("devcontainer-feature.json");
        if !manifest_path.is_file() {
            // Per spec: warn and skip (does not abort). Presentation is left to
            // the CLI; the library only emits a structured tracing event.
            tracing::warn!(
                feature = dir_name,
                "feature is missing a devcontainer-feature.json, skipping"
            );
            continue;
        }

        // install.sh missing → fatal abort per spec.
        if !dir.join("install.sh").is_file() {
            return Err(PackageError::MissingInstallSh {
                feature: dir_name.to_owned(),
            });
        }

        // The official CLI does NOT require the feature `id` to match the
        // subdir name; it only checks `id`/`version`/`name` presence. We mirror
        // that and intentionally do not enforce id == folder name.
        let mut meta = read_manifest(&manifest_path)?;
        let packaged = PackagedFeature::from_meta(&mut meta, Some(dir_name))?;

        tracing::debug!(
            id = packaged.id,
            version = packaged.version,
            dir = dir_name,
            "packaging collection feature"
        );

        let archive_path = output.join(archive_name(&packaged.id));
        write_tgz(dir, &archive_path, &packaged.raw)?;

        tracing::debug!(id = packaged.id, archive = %archive_path.display(), "wrote feature archive");

        packaged_features.push(packaged);
    }

    if packaged_features.is_empty() {
        return Err(PackageError::NoFeaturesFound);
    }

    write_collection_json(output, &packaged_features)?;

    Ok(PackageResult {
        output_folder: output.to_owned(),
        features: packaged_features,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn archive_name(id: &str) -> String {
    format!("devcontainer-feature-{id}.tgz")
}

/// Read and parse a `devcontainer-feature.json` manifest.
///
/// Parsed as JSONC (comments and trailing commas stripped) to match the
/// official CLI, which uses `jsonc-parser` for these files.
fn read_manifest(manifest_path: &Path) -> Result<serde_json::Map<String, Value>, PackageError> {
    let raw = fs::read_to_string(manifest_path).map_err(PackageError::Io)?;
    let stripped = cella_jsonc::strip(&raw).map_err(|e| PackageError::InvalidJson {
        path: manifest_path.to_owned(),
        reason: e.to_string(),
    })?;
    serde_json::from_str(&stripped).map_err(|e| PackageError::InvalidJson {
        path: manifest_path.to_owned(),
        reason: e.to_string(),
    })
}

/// If `legacyIds` is present and non-empty, inject `currentId` into `meta`.
fn inject_current_id_if_needed(meta: &mut serde_json::Map<String, Value>, id: &str) {
    let has_legacy = meta
        .get("legacyIds")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| !arr.is_empty());
    if has_legacy {
        tracing::trace!(id, "injecting currentId for legacyIds");
        meta.insert("currentId".to_owned(), Value::String(id.to_owned()));
    }
}

/// Read a non-empty string field from `meta`.
fn non_empty_str(meta: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl PackagedFeature {
    /// Validate required fields, inject `currentId` if needed, and build a
    /// [`PackagedFeature`] from the parsed manifest map.
    ///
    /// `feature_name` is the collection subdir name (collection mode) or `None`
    /// (single mode), used only for error messages.
    ///
    /// # Errors
    ///
    /// Returns [`PackageError::MissingRequiredFields`] if `id`, `version`, or
    /// `name` are absent or empty.
    fn from_meta(
        meta: &mut serde_json::Map<String, Value>,
        feature_name: Option<&str>,
    ) -> Result<Self, PackageError> {
        let (Some(id), Some(version), Some(name)) = (
            non_empty_str(meta, "id"),
            non_empty_str(meta, "version"),
            non_empty_str(meta, "name"),
        ) else {
            return Err(PackageError::MissingRequiredFields {
                feature: feature_name.unwrap_or("(single)").to_owned(),
            });
        };
        let description = non_empty_str(meta, "description");

        inject_current_id_if_needed(meta, &id);

        Ok(Self {
            id,
            version,
            name: Some(name),
            description,
            raw: meta.clone(),
        })
    }
}

/// Write a `.tgz` of `source_dir` with all paths relative to `.`.
///
/// The metadata map is written back into the archive as the updated
/// `devcontainer-feature.json` (e.g., with `currentId` injected).
fn write_tgz(
    source_dir: &Path,
    archive_path: &Path,
    meta: &serde_json::Map<String, Value>,
) -> Result<(), PackageError> {
    // The default output folder (`./output`) commonly lives inside the source
    // tree. Resolve it so the recursive walk can skip it — otherwise the
    // in-progress archive would package itself, producing a corrupt tarball.
    let skip_dir = archive_path.parent().and_then(|p| p.canonicalize().ok());

    let file = fs::File::create(archive_path).map_err(PackageError::Io)?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(gz);

    // Walk the directory and append every entry.
    append_dir_recursive(&mut tar, source_dir, Path::new("."), skip_dir.as_deref())?;

    // Override devcontainer-feature.json inside the archive with the (possibly
    // mutated) metadata map. Always use ./- prefix for consistency.
    let updated_json =
        serde_json::to_string_pretty(&meta).map_err(|e| PackageError::Serialize(e.to_string()))?;
    let bytes = updated_json.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(
        u64::try_from(bytes.len()).map_err(|_| {
            PackageError::Serialize("metadata JSON exceeds u64 size limit".to_owned())
        })?,
    );
    header.set_mode(0o644);
    append_with_path(
        &mut tar,
        header,
        Path::new("./devcontainer-feature.json"),
        bytes,
    )?;

    tar.into_inner()
        .map_err(PackageError::Io)?
        .finish()
        .map_err(PackageError::Io)?;

    Ok(())
}

/// Recursively append all regular files in `src` to the tar builder under `base`.
///
/// - All entry paths are `./`-prefixed for consistency.
/// - File modes are preserved from the source filesystem.
/// - Symlinks are skipped with a warning (not followed, to prevent traversal).
/// - The output directory (`skip_dir`, canonicalized) is skipped so an
///   output folder nested in the source tree never packages itself.
/// - Special files (FIFOs, sockets, device nodes) are skipped with a warning.
fn append_dir_recursive<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    base: &Path,
    skip_dir: Option<&Path>,
) -> Result<(), PackageError> {
    for entry in fs::read_dir(src).map_err(PackageError::Io)? {
        let entry = entry.map_err(PackageError::Io)?;
        let file_name = entry.file_name();
        let rel = base.join(&file_name);
        let path = entry.path();
        // Use non-following file_type() to detect symlinks without resolving them.
        let file_type = entry.file_type().map_err(PackageError::Io)?;

        // Skip the manifest — we re-append it separately (with mutations).
        if base == Path::new(".") && file_name == "devcontainer-feature.json" {
            continue;
        }

        if file_type.is_symlink() {
            tracing::warn!(
                path = %path.display(),
                "skipping symlink during feature packaging"
            );
            continue;
        }

        if file_type.is_dir() {
            // Skip the output directory if it lives inside the source tree.
            if skip_dir.is_some_and(|skip| path.canonicalize().is_ok_and(|p| p == skip)) {
                tracing::debug!(
                    path = %path.display(),
                    "skipping output directory nested in source tree"
                );
                continue;
            }
            append_dir_recursive(tar, &path, &rel, skip_dir)?;
        } else if file_type.is_file() {
            append_file_with_mode(tar, &path, &rel)?;
        } else {
            // FIFOs, sockets, block/char devices — not packageable. The official
            // CLI walks a plain directory copy and would choke on these too;
            // skipping with a warning is the safe, drop-in-friendly behavior.
            tracing::warn!(
                path = %path.display(),
                "skipping non-regular file during feature packaging"
            );
        }
    }
    Ok(())
}

/// Append a single file to the tar archive, preserving its Unix mode.
fn append_file_with_mode<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    tar_path: &Path,
) -> Result<(), PackageError> {
    let metadata = fs::metadata(src).map_err(PackageError::Io)?;
    let mut file = fs::File::open(src).map_err(PackageError::Io)?;

    let mut header = tar::Header::new_gnu();
    header.set_metadata(&metadata);

    tracing::trace!(path = %tar_path.display(), "appending file to archive");

    append_with_path(tar, header, tar_path, &mut file)
}

/// Append an entry whose archive path keeps its leading `./` prefix.
///
/// `Header::set_path` (and thus `Builder::append_data`) strips leading `CurDir`
/// components, dropping the `./` the official CLI emits. To preserve it we write
/// the path bytes straight into the header `name` field when they fit, and fall
/// back to a GNU long-name (`L`) extension entry when they don't — the same
/// mechanism the `tar` crate uses internally, which keeps the full `./`-prefixed
/// path in the extension's payload.
fn append_with_path<W: std::io::Write, R: std::io::Read>(
    tar: &mut tar::Builder<W>,
    mut header: tar::Header,
    tar_path: &Path,
    data: R,
) -> Result<(), PackageError> {
    let path_str = tar_path.to_str().ok_or_else(|| {
        PackageError::Serialize(format!("non-UTF-8 path: {}", tar_path.display()))
    })?;
    let bytes = path_str.as_bytes();

    let name_len = header
        .as_gnu()
        .ok_or_else(|| PackageError::Serialize("expected GNU tar header".to_owned()))?
        .name
        .len();

    // The name field stores the full path verbatim when it fits (keeping the
    // leading `./`). When it doesn't, emit a GNU long-name (`L`) extension entry
    // carrying the full path first, then store a truncated name that extractors
    // ignore in favor of the extension.
    let name_bytes = if bytes.len() < name_len {
        bytes
    } else {
        append_gnu_long_name(tar, bytes)?;
        truncate_utf8(bytes, name_len - 1)
    };

    let gnu = header
        .as_gnu_mut()
        .ok_or_else(|| PackageError::Serialize("expected GNU tar header".to_owned()))?;
    gnu.name[..name_bytes.len()].copy_from_slice(name_bytes);
    // Zero-terminate (the rest of the array is already zeroed).
    gnu.name[name_bytes.len()] = 0;
    header.set_cksum();
    tar.append(&header, data).map_err(PackageError::Io)?;
    Ok(())
}

/// Emit a GNU `././@LongLink` (type `L`) header + payload carrying `path_bytes`.
fn append_gnu_long_name<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    path_bytes: &[u8],
) -> Result<(), PackageError> {
    let mut header = tar::Header::new_gnu();
    {
        let gnu = header
            .as_gnu_mut()
            .ok_or_else(|| PackageError::Serialize("expected GNU tar header".to_owned()))?;
        let name = b"././@LongLink";
        gnu.name[..name.len()].copy_from_slice(name);
    }
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::GNULongName);
    // Size includes the trailing NUL terminator (GNU tar convention).
    let size = u64::try_from(path_bytes.len() + 1)
        .map_err(|_| PackageError::Serialize("path length exceeds u64 size limit".to_owned()))?;
    header.set_size(size);
    header.set_cksum();

    let payload = std::io::Read::chain(path_bytes, &[0u8][..]);
    tar.append(&header, payload).map_err(PackageError::Io)?;
    Ok(())
}

/// Truncate `bytes` to at most `max` bytes without splitting a UTF-8 sequence.
fn truncate_utf8(bytes: &[u8], max: usize) -> &[u8] {
    if bytes.len() <= max {
        return bytes;
    }
    match std::str::from_utf8(&bytes[..max]) {
        Ok(_) => &bytes[..max],
        Err(e) => &bytes[..e.valid_up_to()],
    }
}

/// Serialize and write `devcontainer-collection.json` to `output`.
fn write_collection_json(output: &Path, features: &[PackagedFeature]) -> Result<(), PackageError> {
    let feature_values: Vec<Value> = features
        .iter()
        .map(|f| {
            // Start from the full metadata map (which already has id, version, etc.)
            // and ensure top-level `id`/`version`/`name`/`description` are present.
            // Using `raw` directly avoids any risk of duplicate keys.
            let mut map = f.raw.clone();
            map.insert("id".to_owned(), Value::String(f.id.clone()));
            map.insert("version".to_owned(), Value::String(f.version.clone()));
            if let Some(n) = &f.name {
                map.insert("name".to_owned(), Value::String(n.clone()));
            }
            if let Some(d) = &f.description {
                map.insert("description".to_owned(), Value::String(d.clone()));
            }
            Value::Object(map)
        })
        .collect();

    let collection = CollectionJson {
        source_information: SourceInformation {
            source: "devcontainer-cli".to_owned(),
            owner: None,
            repo: None,
            tag: None,
            r#ref: None,
            sha: None,
        },
        features: feature_values,
    };

    tracing::debug!(
        output = %output.display(),
        n = features.len(),
        "writing devcontainer-collection.json"
    );

    let json = serde_json::to_string_pretty(&collection)
        .map_err(|e| PackageError::Serialize(e.to_string()))?;
    let collection_path = output.join("devcontainer-collection.json");
    fs::write(collection_path, json).map_err(PackageError::Io)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors specific to the `features package` operation.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error(
        "(!) ERR: Output directory '{}' already exists. Manually delete, or pass '-f' to continue.",
        path.display()
    )]
    OutputFolderExists { path: PathBuf },

    #[error("feature '{feature}' is missing an install.sh")]
    MissingInstallSh { feature: String },

    #[error(
        "feature '{feature}' is missing one of the following required properties in its devcontainer-feature.json: 'id', 'version', 'name'."
    )]
    MissingRequiredFields { feature: String },

    #[error("no packageable features found in target directory")]
    NoFeaturesFound,

    #[error("invalid JSON in {}: {reason}", path.display())]
    InvalidJson { path: PathBuf, reason: String },

    #[error("serialization error: {0}")]
    Serialize(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<PackageError> for FeatureError {
    fn from(e: PackageError) -> Self {
        Self::Io(std::io::Error::other(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use flate2::read::GzDecoder;
    use tar::Archive;
    use tempfile::tempdir;

    use super::*;

    // -----------------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------------

    fn make_feature(dir: &Path, id: &str, version: &str) {
        fs::create_dir_all(dir).unwrap();
        let manifest = serde_json::json!({
            "id": id,
            "version": version,
            "name": format!("Feature {id}"),
            "description": "A test feature"
        });
        fs::write(
            dir.join("devcontainer-feature.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(dir.join("install.sh"), "#!/bin/sh\necho hello\n").unwrap();
    }

    fn tgz_entries(path: &Path) -> HashSet<String> {
        let file = fs::File::open(path).unwrap();
        let gz = GzDecoder::new(file);
        let mut ar = Archive::new(gz);
        ar.entries()
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect()
    }

    fn tgz_entry_modes(path: &Path) -> std::collections::HashMap<String, u32> {
        let file = fs::File::open(path).unwrap();
        let gz = GzDecoder::new(file);
        let mut ar = Archive::new(gz);
        ar.entries()
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| {
                let mode = e.header().mode().unwrap_or(0);
                let name = e.path().unwrap().to_string_lossy().to_string();
                (name, mode)
            })
            .collect()
    }

    fn read_collection(output: &Path) -> Value {
        let raw = fs::read_to_string(output.join("devcontainer-collection.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    // -----------------------------------------------------------------------
    // single-feature mode
    // -----------------------------------------------------------------------

    #[test]
    fn single_feature_creates_tgz_and_collection() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("my-feature");
        make_feature(&src, "my-feature", "1.0.0");

        let out = tmp.path().join("output");
        let result = package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        assert_eq!(result.features.len(), 1);
        assert_eq!(result.features[0].id, "my-feature");

        let tgz = out.join("devcontainer-feature-my-feature.tgz");
        assert!(tgz.exists(), "tgz not found");

        let entries = tgz_entries(&tgz);
        assert!(
            entries.contains("./install.sh"),
            "install.sh missing or not ./- prefixed: {entries:?}"
        );
        assert!(
            entries.contains("./devcontainer-feature.json"),
            "manifest missing or not ./- prefixed: {entries:?}"
        );
        // Every entry must start with ./
        for entry in &entries {
            assert!(
                entry.starts_with("./"),
                "tar entry does not start with ./: {entry:?}"
            );
        }

        let col = read_collection(&out);
        assert_eq!(col["sourceInformation"]["source"], "devcontainer-cli");
        let features = col["features"].as_array().unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0]["id"], "my-feature");
        assert_eq!(features[0]["version"], "1.0.0");
    }

    // -----------------------------------------------------------------------
    // Fix 1: all tgz entries are ./- prefixed
    // -----------------------------------------------------------------------

    #[test]
    fn all_tgz_entries_have_dot_slash_prefix() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        make_feature(&src, "feature", "1.0.0");
        // Add a subdirectory with a file to also test recursive entries.
        fs::create_dir_all(src.join("scripts")).unwrap();
        fs::write(src.join("scripts").join("helper.sh"), "#!/bin/sh").unwrap();

        let out = tmp.path().join("output");
        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        let tgz = out.join("devcontainer-feature-feature.tgz");
        let entries = tgz_entries(&tgz);
        assert!(!entries.is_empty(), "no entries in tgz");
        for entry in &entries {
            assert!(
                entry.starts_with("./"),
                "tar entry does not start with ./: {entry:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Fix 2: file modes are preserved
    // -----------------------------------------------------------------------

    #[test]
    fn executable_install_sh_mode_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        make_feature(&src, "feature", "1.0.0");

        // Make install.sh executable.
        let install_sh = src.join("install.sh");
        fs::set_permissions(&install_sh, fs::Permissions::from_mode(0o755)).unwrap();

        let out = tmp.path().join("output");
        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        let tgz = out.join("devcontainer-feature-feature.tgz");
        let modes = tgz_entry_modes(&tgz);
        let install_mode = modes
            .get("./install.sh")
            .copied()
            .expect("./install.sh not found in archive");
        assert_ne!(
            install_mode & 0o111,
            0,
            "install.sh should be executable, got mode {install_mode:#o}"
        );
    }

    // -----------------------------------------------------------------------
    // Fix 3: symlinks are not followed during packaging
    // -----------------------------------------------------------------------

    #[test]
    fn symlinks_are_skipped_not_followed() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        make_feature(&src, "feature", "1.0.0");

        // Create an outside directory with a sentinel file.
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "should not appear").unwrap();

        // Symlink from inside feature dir to outside.
        std::os::unix::fs::symlink(&outside, src.join("linked")).unwrap();

        let out = tmp.path().join("output");
        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        let tgz = out.join("devcontainer-feature-feature.tgz");
        let entries = tgz_entries(&tgz);

        // No entry should reference the outside dir's files.
        for entry in &entries {
            assert!(
                !entry.contains("secret.txt"),
                "symlink traversal: found outside file {entry:?} in archive"
            );
            assert!(
                !entry.contains("linked"),
                "symlink itself should not appear in archive: {entry:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // collection mode
    // -----------------------------------------------------------------------

    #[test]
    fn collection_packages_multiple_features() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        make_feature(&src.join("feat-a"), "feat-a", "1.0.0");
        make_feature(&src.join("feat-b"), "feat-b", "2.3.0");

        let out = tmp.path().join("output");
        let result = package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        assert_eq!(result.features.len(), 2);
        assert!(out.join("devcontainer-feature-feat-a.tgz").exists());
        assert!(out.join("devcontainer-feature-feat-b.tgz").exists());

        let col = read_collection(&out);
        let features = col["features"].as_array().unwrap();
        assert_eq!(features.len(), 2);
        let ids: Vec<&str> = features.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"feat-a"));
        assert!(ids.contains(&"feat-b"));
    }

    // -----------------------------------------------------------------------
    // validation errors
    // -----------------------------------------------------------------------

    #[test]
    fn missing_version_returns_error() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("bad-feature");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("devcontainer-feature.json"),
            r#"{"id": "bad-feature", "name": "Bad"}"#,
        )
        .unwrap();
        fs::write(src.join("install.sh"), "#!/bin/sh").unwrap();

        let out = tmp.path().join("output");
        let err = package(&PackageOptions {
            target: src,
            output_folder: out,
            force_clean_output_folder: false,
        })
        .unwrap_err();

        assert!(
            matches!(err, PackageError::MissingRequiredFields { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn collection_missing_install_sh_aborts() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        // feat-a is fine
        make_feature(&src.join("feat-a"), "feat-a", "1.0.0");
        // feat-b has no install.sh
        fs::create_dir_all(src.join("feat-b")).unwrap();
        fs::write(
            src.join("feat-b").join("devcontainer-feature.json"),
            r#"{"id": "feat-b", "version": "1.0.0", "name": "B"}"#,
        )
        .unwrap();

        let out = tmp.path().join("output");
        let err = package(&PackageOptions {
            target: src,
            output_folder: out,
            force_clean_output_folder: false,
        })
        .unwrap_err();

        assert!(
            matches!(err, PackageError::MissingInstallSh { .. }),
            "unexpected error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // --force-clean-output-folder
    // -----------------------------------------------------------------------

    #[test]
    fn existing_output_without_force_returns_error() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("f");
        make_feature(&src, "f", "1.0.0");
        let out = tmp.path().join("output");
        fs::create_dir_all(&out).unwrap();

        let err = package(&PackageOptions {
            target: src,
            output_folder: out,
            force_clean_output_folder: false,
        })
        .unwrap_err();

        assert!(
            matches!(err, PackageError::OutputFolderExists { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn force_clean_removes_existing_output() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("f");
        make_feature(&src, "f", "1.0.0");
        let out = tmp.path().join("output");
        // Pre-create output with a stale file.
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("stale.tgz"), b"old").unwrap();

        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: true,
        })
        .unwrap();

        // Stale file must be gone.
        assert!(!out.join("stale.tgz").exists());
        // New artifact must be present.
        assert!(out.join("devcontainer-feature-f.tgz").exists());
    }

    // -----------------------------------------------------------------------
    // legacyIds → currentId injection
    // -----------------------------------------------------------------------

    #[test]
    fn legacy_ids_injects_current_id_in_tgz() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("new-id");
        fs::create_dir_all(&src).unwrap();
        let manifest = serde_json::json!({
            "id": "new-id",
            "version": "2.0.0",
            "name": "New ID",
            "legacyIds": ["old-id"]
        });
        fs::write(
            src.join("devcontainer-feature.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(src.join("install.sh"), "#!/bin/sh").unwrap();

        let out = tmp.path().join("output");
        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        // Extract manifest from tgz and verify currentId.
        let tgz_path = out.join("devcontainer-feature-new-id.tgz");
        let file = fs::File::open(&tgz_path).unwrap();
        let gz = GzDecoder::new(file);
        let mut ar = Archive::new(gz);
        let mut found = false;
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path.contains("devcontainer-feature.json") {
                let mut contents = String::new();
                std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
                let v: Value = serde_json::from_str(&contents).unwrap();
                assert_eq!(v["currentId"], "new-id");
                found = true;
                break;
            }
        }
        assert!(found, "devcontainer-feature.json not found in archive");
    }

    // -----------------------------------------------------------------------
    // hidden dirs are skipped in collection mode
    // -----------------------------------------------------------------------

    #[test]
    fn collection_skips_hidden_directories() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        make_feature(&src.join("real-feature"), "real-feature", "1.0.0");
        // Hidden dir — should be silently skipped.
        fs::create_dir_all(src.join(".hidden")).unwrap();
        fs::write(
            src.join(".hidden").join("devcontainer-feature.json"),
            r#"{"id": "hidden", "version": "1.0.0", "name": "Hidden"}"#,
        )
        .unwrap();

        let out = tmp.path().join("output");
        let result = package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        assert_eq!(result.features.len(), 1);
        assert_eq!(result.features[0].id, "real-feature");
        assert!(!out.join("devcontainer-feature-hidden.tgz").exists());
    }

    // -----------------------------------------------------------------------
    // Fix 4: collection JSON shape stays correct (no duplicate keys)
    // -----------------------------------------------------------------------

    #[test]
    fn collection_json_shape_is_correct() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        make_feature(&src, "my-feat", "3.0.0");

        let out = tmp.path().join("output");
        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        let col = read_collection(&out);
        // Top-level keys must exist.
        assert!(col["sourceInformation"].is_object());
        assert_eq!(col["sourceInformation"]["source"], "devcontainer-cli");
        assert!(col["features"].is_array());
        let features = col["features"].as_array().unwrap();
        assert_eq!(features.len(), 1);
        // Each feature entry must carry full metadata.
        assert_eq!(features[0]["id"], "my-feat");
        assert_eq!(features[0]["version"], "3.0.0");
        assert!(features[0]["name"].is_string());
    }

    // -----------------------------------------------------------------------
    // Regression: output folder nested in the source tree is not packaged
    // into its own (self-referential) archive.
    // -----------------------------------------------------------------------

    #[test]
    fn output_folder_inside_source_tree_is_not_self_referenced() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        make_feature(&src, "feature", "1.0.0");

        // Default-style layout: output folder lives directly under the source.
        let out = src.join("output");

        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        let tgz = out.join("devcontainer-feature-feature.tgz");
        let entries = tgz_entries(&tgz);

        // No entry may reference the output folder or the tarball itself.
        for entry in &entries {
            assert!(
                !entry.contains("output"),
                "output folder leaked into archive: {entry:?}"
            );
            assert!(
                !entry.contains(".tgz"),
                "archive packaged itself: {entry:?}"
            );
        }
        // Sanity: the real feature files are still present.
        assert!(entries.contains("./install.sh"));
        assert!(entries.contains("./devcontainer-feature.json"));
    }

    // -----------------------------------------------------------------------
    // Regression: deeply-nested files whose archive path exceeds the 100-byte
    // ustar name field package correctly via the GNU long-name extension.
    // -----------------------------------------------------------------------

    #[test]
    fn long_nested_paths_package_via_gnu_long_name() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        make_feature(&src, "feature", "1.0.0");

        // Build a path whose `./`-prefixed form is well over 100 bytes.
        let deep = src
            .join("a-rather-long-directory-name-segment-number-one")
            .join("a-rather-long-directory-name-segment-number-two")
            .join("a-rather-long-directory-name-segment-number-three");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deeply-nested-helper-script.sh"), "#!/bin/sh\n").unwrap();

        let out = tmp.path().join("output");
        package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        let tgz = out.join("devcontainer-feature-feature.tgz");
        let expected = "./a-rather-long-directory-name-segment-number-one/\
                        a-rather-long-directory-name-segment-number-two/\
                        a-rather-long-directory-name-segment-number-three/\
                        deeply-nested-helper-script.sh";
        assert!(
            expected.len() >= 100,
            "test path must exceed the ustar 100-byte field to exercise the fix"
        );

        let entries = tgz_entries(&tgz);
        assert!(
            entries.contains(expected),
            "long nested path missing or mangled: {entries:?}"
        );
        // The `./` prefix must survive the long-name path.
        for entry in &entries {
            assert!(
                entry.starts_with("./"),
                "tar entry does not start with ./: {entry:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Regression: manifests with comments and trailing commas (JSONC) are
    // accepted, matching the official CLI's jsonc-parser.
    // -----------------------------------------------------------------------

    #[test]
    fn jsonc_manifest_with_comments_and_trailing_commas_is_accepted() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("feature");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("devcontainer-feature.json"),
            r#"{
                // leading line comment
                "id": "feature",
                "version": "1.0.0",
                "name": "Feature", /* inline block comment */
                "description": "A test feature",
            }"#,
        )
        .unwrap();
        fs::write(src.join("install.sh"), "#!/bin/sh\n").unwrap();

        let out = tmp.path().join("output");
        let result = package(&PackageOptions {
            target: src,
            output_folder: out.clone(),
            force_clean_output_folder: false,
        })
        .unwrap();

        assert_eq!(result.features.len(), 1);
        assert_eq!(result.features[0].id, "feature");

        // The re-emitted manifest inside the tarball must be plain JSON.
        let col = read_collection(&out);
        assert_eq!(col["features"][0]["id"], "feature");
        assert_eq!(col["features"][0]["version"], "1.0.0");
    }
}
