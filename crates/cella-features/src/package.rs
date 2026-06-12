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

    // Guard: output folder must not already exist (or force-clean it).
    if output.exists() {
        if opts.force_clean_output_folder {
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
        package_single(target, output)
    } else {
        package_collection(target, output)
    }
}

// ---------------------------------------------------------------------------
// Single-feature mode
// ---------------------------------------------------------------------------

fn package_single(target: &Path, output: &Path) -> Result<PackageResult, PackageError> {
    let manifest_path = target.join("devcontainer-feature.json");
    let raw = fs::read_to_string(&manifest_path).map_err(PackageError::Io)?;
    let mut meta: serde_json::Map<String, Value> =
        serde_json::from_str(&raw).map_err(|e| PackageError::InvalidJson {
            path: manifest_path.clone(),
            reason: e.to_string(),
        })?;

    validate_required_fields(&meta, None)?;

    let id = meta["id"].as_str().unwrap().to_owned();
    let version = meta["version"].as_str().unwrap().to_owned();
    let name = meta.get("name").and_then(|v| v.as_str()).map(str::to_owned);
    let description = meta
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    inject_current_id_if_needed(&mut meta, &id);

    let archive_name = archive_name(&id);
    let archive_path = output.join(&archive_name);
    write_tgz(target, &archive_path, &meta)?;

    let packaged = PackagedFeature {
        id,
        version,
        name,
        description,
        raw: meta,
    };

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
            continue;
        }

        let manifest_path = dir.join("devcontainer-feature.json");
        if !manifest_path.is_file() {
            // Per spec: warn and skip (does not abort).
            eprintln!(
                "(!) WARNING: feature '{dir_name}' is missing a devcontainer-feature.json. Skipping... "
            );
            continue;
        }

        // install.sh missing → fatal abort per spec.
        if !dir.join("install.sh").is_file() {
            return Err(PackageError::MissingInstallSh {
                feature: dir_name.to_owned(),
            });
        }

        let raw = fs::read_to_string(&manifest_path).map_err(PackageError::Io)?;
        let mut meta: serde_json::Map<String, Value> =
            serde_json::from_str(&raw).map_err(|e| PackageError::InvalidJson {
                path: manifest_path.clone(),
                reason: e.to_string(),
            })?;

        validate_required_fields(&meta, Some(dir_name))?;

        let id = meta["id"].as_str().unwrap().to_owned();
        let version = meta["version"].as_str().unwrap().to_owned();
        let name = meta.get("name").and_then(|v| v.as_str()).map(str::to_owned);
        let description = meta
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        inject_current_id_if_needed(&mut meta, &id);

        let archive_path = output.join(archive_name(&id));
        write_tgz(dir, &archive_path, &meta)?;

        packaged_features.push(PackagedFeature {
            id,
            version,
            name,
            description,
            raw: meta,
        });
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

/// If `legacyIds` is present and non-empty, inject `currentId` into `meta`.
fn inject_current_id_if_needed(meta: &mut serde_json::Map<String, Value>, id: &str) {
    let has_legacy = meta
        .get("legacyIds")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| !arr.is_empty());
    if has_legacy {
        meta.insert("currentId".to_owned(), Value::String(id.to_owned()));
    }
}

/// Validate that `id`, `version`, and `name` are all present and non-empty.
fn validate_required_fields(
    meta: &serde_json::Map<String, Value>,
    feature_name: Option<&str>,
) -> Result<(), PackageError> {
    let ok = meta
        .get("id")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
        && meta
            .get("version")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
        && meta
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());

    if ok {
        Ok(())
    } else {
        Err(PackageError::MissingRequiredFields {
            feature: feature_name.unwrap_or("(single)").to_owned(),
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
    let file = fs::File::create(archive_path).map_err(PackageError::Io)?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(gz);

    // Walk the directory and append every entry.
    append_dir_recursive(&mut tar, source_dir, Path::new("."))?;

    // Override devcontainer-feature.json inside the archive with the (possibly
    // mutated) metadata map.
    let updated_json =
        serde_json::to_string_pretty(&meta).map_err(|e| PackageError::Serialize(e.to_string()))?;
    let bytes = updated_json.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "./devcontainer-feature.json", bytes)
        .map_err(PackageError::Io)?;

    tar.into_inner()
        .map_err(PackageError::Io)?
        .finish()
        .map_err(PackageError::Io)?;

    Ok(())
}

/// Recursively append all files in `src` to the tar builder under `base`.
fn append_dir_recursive<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    base: &Path,
) -> Result<(), PackageError> {
    for entry in fs::read_dir(src).map_err(PackageError::Io)? {
        let entry = entry.map_err(PackageError::Io)?;
        let path = entry.path();
        let file_name = entry.file_name();
        let rel = base.join(&file_name);

        // Skip the manifest — we re-append it separately (with mutations).
        if base == Path::new(".") && file_name == "devcontainer-feature.json" {
            continue;
        }

        if path.is_dir() {
            append_dir_recursive(tar, &path, &rel)?;
        } else {
            let mut f = fs::File::open(&path).map_err(PackageError::Io)?;
            tar.append_file(&rel, &mut f).map_err(PackageError::Io)?;
        }
    }
    Ok(())
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
            entries.contains("./install.sh") || entries.contains("install.sh"),
            "install.sh missing from tgz: {entries:?}"
        );
        assert!(
            entries.contains("./devcontainer-feature.json")
                || entries.contains("devcontainer-feature.json"),
            "manifest missing from tgz: {entries:?}"
        );

        let col = read_collection(&out);
        assert_eq!(col["sourceInformation"]["source"], "devcontainer-cli");
        let features = col["features"].as_array().unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0]["id"], "my-feature");
        assert_eq!(features[0]["version"], "1.0.0");
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
}
