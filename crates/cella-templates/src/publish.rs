//! `cella templates publish` — package and publish templates to an OCI registry.
//!
//! Implements the same contract as `devcontainer templates publish`:
//!
//! - Detects single-template vs collection based on whether a
//!   `devcontainer-template.json` exists at the target root.
//! - Computes `type`, `files`, `fileCount`, and `featureIds` metadata fields
//!   by inspecting the template's `devcontainer.json`.
//! - Packages each template as `devcontainer-template-<id>.tgz` with `./` entry
//!   prefixes (tar `cwd`-relative entries).
//! - Semver fan-out: version `1.2.3` → tags `["1.2.3", "1.2", "1", "latest"]`,
//!   where a major/minor/`latest` tag is only pushed when the new version is
//!   the highest satisfying that range among already-published tags.
//! - Version-skip semantics: no `version` field → warn + skip; exact version
//!   already published → skip (OCI tag list check).
//! - Collection index published as `devcontainer-collection.json` artifact.
//! - Stdout JSON: `{"<id>": {"digest": "…", "publishedTags": […], "version": "…"}}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use crate::types::TemplateMetadata;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from the publish pipeline.
#[derive(Debug, Error)]
pub enum PublishError {
    #[error("I/O error for template '{id}': {source}")]
    Io {
        id: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid template metadata for '{id}': {reason}")]
    InvalidMetadata { id: String, reason: String },

    #[error("missing devcontainer.json in template '{id}': {reason}")]
    MissingDevcontainerJson { id: String, reason: String },

    #[error("invalid version '{version}' for template '{id}': {reason}")]
    InvalidVersion {
        id: String,
        version: String,
        reason: String,
    },

    #[error("packaging failed for template '{id}': {source}")]
    PackagingFailed {
        id: String,
        #[source]
        source: std::io::Error,
    },

    #[error("OCI push failed for template '{id}': {source}")]
    PushFailed {
        id: String,
        #[source]
        source: Box<cella_oci::push::PushError>,
    },

    #[error("registry tag list failed for '{reference}': {source}")]
    TagListFailed {
        reference: String,
        #[source]
        source: Box<cella_oci::push::PushError>,
    },
}

// ---------------------------------------------------------------------------
// Public options / results
// ---------------------------------------------------------------------------

/// Input options for [`publish_templates`].
pub struct PublishOptions {
    /// Target folder — either a collection directory or a single template dir.
    /// Defaults to `"."` (current directory).
    pub target: PathBuf,

    /// OCI registry hostname, e.g. `ghcr.io`.
    pub registry: String,

    /// OCI namespace (owner/org), e.g. `myorg/templates`.
    pub namespace: String,
}

/// Outcome for a single template within a publish run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplatePublishResult {
    /// Whether this template was skipped (missing version or already published).
    pub skipped: bool,

    /// Manifest digest, present only when the template was actually pushed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,

    /// Tags pushed to the registry, present only when the template was pushed.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub published_tags: Vec<String>,

    /// Semver version string, present only when the template was pushed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Aggregated output of a publish run.
pub type PublishOutput = HashMap<String, TemplatePublishResult>;

// ---------------------------------------------------------------------------
// Internal representation of enriched template metadata
// ---------------------------------------------------------------------------

/// Template type derived from the devcontainer config inside the template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TemplateType {
    Image,
    Dockerfile,
    DockerCompose,
}

/// Metadata sent in the OCI manifest annotation, per the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnnotationMetadata {
    #[serde(flatten)]
    base: TemplateMetadata,
    #[serde(rename = "type")]
    template_type: TemplateType,
    files: Vec<String>,
    file_count: usize,
    feature_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Package and publish templates from `opts.target` to the OCI registry.
///
/// Returns a map from template ID to publish result. Callers should serialize
/// this to JSON and print to stdout — that matches the official CLI contract.
///
/// # Errors
///
/// Returns [`PublishError`] for unrecoverable failures. Per-template skip
/// decisions (missing version, already published) are recorded in the result
/// map with `skipped: true` rather than propagating errors.
pub async fn publish_templates(opts: PublishOptions) -> Result<PublishOutput, PublishError> {
    let target = opts
        .target
        .canonicalize()
        .map_err(|source| PublishError::Io {
            id: "<target>".to_owned(),
            source,
        })?;

    let is_single = is_single_template(&target);
    debug!("target={} is_single={is_single}", target.display());

    let template_dirs = if is_single {
        vec![target.clone()]
    } else {
        collect_collection_dirs(&target)?
    };

    let tmp_path = {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!("cella-tpl-publish-{ts}"))
    };
    std::fs::create_dir_all(&tmp_path).map_err(|source| PublishError::Io {
        id: "<tmpdir>".to_owned(),
        source,
    })?;

    let mut output: PublishOutput = HashMap::new();

    for dir in &template_dirs {
        let id = dir.file_name().map_or_else(
            || "unknown".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );

        let result =
            publish_single_template(&id, dir, &tmp_path, &opts.registry, &opts.namespace).await;

        match result {
            Ok(r) => {
                output.insert(id, r);
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    // Publish collection index
    let index_result = publish_collection_index(
        &opts.registry,
        &opts.namespace,
        &output,
        &template_dirs,
        &target,
    )
    .await;

    // Best-effort cleanup of tmp directory.
    let _ = std::fs::remove_dir_all(&tmp_path);

    index_result?;
    Ok(output)
}

// ---------------------------------------------------------------------------
// Collection index
// ---------------------------------------------------------------------------

async fn publish_collection_index(
    registry: &str,
    namespace: &str,
    output: &PublishOutput,
    template_dirs: &[PathBuf],
    target: &Path,
) -> Result<(), PublishError> {
    let mut templates_meta: Vec<serde_json::Value> = Vec::new();

    for dir in template_dirs {
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Skip templates that failed or were skipped
        let Some(result) = output.get(&id) else {
            continue;
        };
        if result.skipped {
            continue;
        }

        let json_path = dir.join("devcontainer-template.json");
        if !json_path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&json_path).map_err(|source| PublishError::Io {
            id: id.clone(),
            source,
        })?;
        let meta: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| PublishError::InvalidMetadata {
                id: id.clone(),
                reason: e.to_string(),
            })?;
        templates_meta.push(meta);
    }

    let collection_json = serde_json::json!({
        "sourceInformation": {
            "source": target.to_string_lossy()
        },
        "templates": templates_meta
    });
    let collection_bytes =
        serde_json::to_vec_pretty(&collection_json).expect("collection JSON is serializable");

    let layer = cella_oci::push::LayerSpec {
        data: collection_bytes,
        media_type: "application/vnd.devcontainers.collection.layer.v1+json".to_owned(),
        annotations: Some({
            let mut m = HashMap::new();
            m.insert(
                "org.opencontainers.image.title".to_owned(),
                "devcontainer-collection.json".to_owned(),
            );
            m
        }),
    };

    let repository = namespace.to_owned();
    debug!("publishing collection index to {registry}/{repository}");

    cella_oci::push::push_artifact(
        registry,
        &repository,
        &["latest".to_owned()],
        vec![layer],
        "application/vnd.devcontainers.collection",
        None::<HashMap<String, String>>,
    )
    .await
    .map_err(|source| PublishError::PushFailed {
        id: "<collection>".to_owned(),
        source: Box::new(source),
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Single-template pipeline
// ---------------------------------------------------------------------------

async fn publish_single_template(
    id: &str,
    dir: &Path,
    out_dir: &Path,
    registry: &str,
    namespace: &str,
) -> Result<TemplatePublishResult, PublishError> {
    // Read devcontainer-template.json
    let json_path = dir.join("devcontainer-template.json");
    if !json_path.exists() {
        warn!("template '{id}' is missing devcontainer-template.json, skipping");
        return Ok(TemplatePublishResult {
            skipped: true,
            digest: None,
            published_tags: Vec::new(),
            version: None,
        });
    }

    let raw = std::fs::read_to_string(&json_path).map_err(|source| PublishError::Io {
        id: id.to_owned(),
        source,
    })?;
    let base_meta: TemplateMetadata =
        serde_json::from_str(&raw).map_err(|e| PublishError::InvalidMetadata {
            id: id.to_owned(),
            reason: e.to_string(),
        })?;

    // Version-skip check
    if base_meta.version.is_empty() {
        warn!("(!) WARNING: Version does not exist, skipping {id}...");
        return Ok(TemplatePublishResult {
            skipped: true,
            digest: None,
            published_tags: Vec::new(),
            version: None,
        });
    }

    let version = base_meta.version.clone();
    let repository = format!("{namespace}/{id}");

    // Fetch existing tags
    let existing_tags = cella_oci::push::list_published_tags(registry, &repository)
        .await
        .map_err(|source| PublishError::TagListFailed {
            reference: format!("{registry}/{repository}"),
            source: Box::new(source),
        })?;

    // Compute semver fan-out tags
    let Some(tags) = compute_semver_tags(&version, &existing_tags) else {
        warn!("(!) WARNING: Version {version} already exists, skipping {id}...");
        return Ok(TemplatePublishResult {
            skipped: true,
            digest: None,
            published_tags: Vec::new(),
            version: None,
        });
    };

    // Compute enriched metadata
    let annotation_meta = compute_annotation_metadata(id, dir, base_meta)?;

    // Package template
    let tgz_path = package_template(id, dir, out_dir)?;

    // Build annotation
    let annotation_json =
        serde_json::to_string(&annotation_meta).expect("annotation metadata is serializable");
    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert("dev.containers.metadata".to_owned(), annotation_json);

    // Read tarball bytes
    let tgz_bytes = std::fs::read(&tgz_path).map_err(|source| PublishError::Io {
        id: id.to_owned(),
        source,
    })?;

    let layer = cella_oci::push::LayerSpec {
        data: tgz_bytes,
        media_type: "application/vnd.devcontainers.layer.v1+tar".to_owned(),
        annotations: Some({
            let mut m = HashMap::new();
            m.insert(
                "org.opencontainers.image.title".to_owned(),
                format!("devcontainer-template-{id}.tgz"),
            );
            m
        }),
    };

    debug!("pushing {registry}/{repository} tags={tags:?}");

    let push_result = cella_oci::push::push_artifact(
        registry,
        &repository,
        &tags,
        vec![layer],
        "application/vnd.devcontainers",
        Some(manifest_annotations),
    )
    .await
    .map_err(|source| PublishError::PushFailed {
        id: id.to_owned(),
        source: Box::new(source),
    })?;

    match push_result {
        Some(r) => Ok(TemplatePublishResult {
            skipped: false,
            digest: Some(r.digest),
            published_tags: r.pushed_tags,
            version: Some(version.clone()),
        }),
        None => Ok(TemplatePublishResult {
            skipped: true,
            digest: None,
            published_tags: Vec::new(),
            version: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Annotation metadata computation
// ---------------------------------------------------------------------------

fn compute_annotation_metadata(
    id: &str,
    dir: &Path,
    base: TemplateMetadata,
) -> Result<AnnotationMetadata, PublishError> {
    let dc_path =
        find_devcontainer_json(dir).ok_or_else(|| PublishError::MissingDevcontainerJson {
            id: id.to_owned(),
            reason: "no devcontainer.json or .devcontainer/devcontainer.json found".to_owned(),
        })?;

    let dc_raw = std::fs::read_to_string(&dc_path).map_err(|source| PublishError::Io {
        id: id.to_owned(),
        source,
    })?;

    // Strip JSONC comments before parsing
    let stripped = strip_jsonc_comments(&dc_raw);
    let dc: serde_json::Value =
        serde_json::from_str(&stripped).map_err(|e| PublishError::MissingDevcontainerJson {
            id: id.to_owned(),
            reason: format!("devcontainer.json parse error: {e}"),
        })?;

    let template_type = derive_template_type(&dc)?;
    let files = collect_files(dir);
    let file_count = files.len();
    let feature_ids = extract_feature_ids(&dc);

    Ok(AnnotationMetadata {
        base,
        template_type,
        files,
        file_count,
        feature_ids,
    })
}

/// Derive the template type from the devcontainer config.
///
/// Logic mirrors `addsAdditionalTemplateProps` in the official CLI:
/// - `image` key present → `Image`
/// - `dockerFile`/`build.dockerfile` present → `Dockerfile`
/// - `dockerComposeFile` present → `DockerCompose`
fn derive_template_type(dc: &serde_json::Value) -> Result<TemplateType, PublishError> {
    if dc.get("image").is_some() {
        return Ok(TemplateType::Image);
    }
    if dc.get("dockerFile").is_some() || dc.get("build").and_then(|b| b.get("dockerfile")).is_some()
    {
        return Ok(TemplateType::Dockerfile);
    }
    if dc.get("dockerComposeFile").is_some() {
        return Ok(TemplateType::DockerCompose);
    }
    Err(PublishError::MissingDevcontainerJson {
        id: "?".to_owned(),
        reason: r#"devcontainer.json must contain "image", "dockerFile", or "dockerComposeFile""#
            .to_owned(),
    })
}

/// Extract resolved feature resource IDs from `features` in devcontainer.json.
///
/// The official CLI calls `getRef(output, f)?.resource` for each key — here
/// we just normalize the key form: strip the tag/digest suffix if it looks
/// like a fully-qualified reference, otherwise keep it as-is.
fn extract_feature_ids(dc: &serde_json::Value) -> Vec<String> {
    let Some(features) = dc.get("features").and_then(|f| f.as_object()) else {
        return Vec::new();
    };

    features
        .keys()
        .map(|k| {
            // Normalise to base reference (drop `:tag` suffix) matching OCI resource form.
            let base = k.split_once(':').map_or(k.as_str(), |(base, _)| base);
            base.to_owned()
        })
        .collect()
}

/// Recursively collect all file paths under `dir`, relative to `dir`, sorted.
fn collect_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files_recursive(dir, dir, &mut files);
    files.sort();
    files
}

fn collect_files_recursive(base: &Path, current: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(base, &path, out);
        } else if path.is_file()
            && let Ok(rel) = path.strip_prefix(base)
        {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

/// Locate `devcontainer.json` inside a template directory.
///
/// Checks `.devcontainer.json` at root, then `.devcontainer/devcontainer.json`.
fn find_devcontainer_json(dir: &Path) -> Option<PathBuf> {
    let flat = dir.join(".devcontainer.json");
    if flat.exists() {
        return Some(flat);
    }
    let nested = dir.join(".devcontainer").join("devcontainer.json");
    if nested.exists() {
        return Some(nested);
    }
    None
}

/// Minimal JSONC comment stripper — removes `//` line comments and `/* */` blocks.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;

    while let Some(c) = chars.next() {
        if escape {
            out.push(c);
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            out.push(c);
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    // Line comment — consume through newline
                    for nc in chars.by_ref() {
                        if nc == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    // Block comment — consume through `*/`
                    chars.next(); // consume `*`
                    let mut prev = ' ';
                    for nc in chars.by_ref() {
                        if prev == '*' && nc == '/' {
                            break;
                        }
                        if nc == '\n' {
                            out.push('\n');
                        }
                        prev = nc;
                    }
                }
                _ => {
                    out.push(c);
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Packaging
// ---------------------------------------------------------------------------

/// Package a template directory into `devcontainer-template-<id>.tgz`.
///
/// All entries use `./` prefixes (tar `cwd`-relative), modes are preserved.
/// Symlinks are not followed (tar follows them by default; we disable that).
fn package_template(id: &str, dir: &Path, out_dir: &Path) -> Result<PathBuf, PublishError> {
    let archive_name = format!("devcontainer-template-{id}.tgz");
    let out_path = out_dir.join(&archive_name);

    let file =
        std::fs::File::create(&out_path).map_err(|source| PublishError::PackagingFailed {
            id: id.to_owned(),
            source,
        })?;

    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);

    builder
        .append_dir_all(".", dir)
        .map_err(|source| PublishError::PackagingFailed {
            id: id.to_owned(),
            source,
        })?;

    builder
        .finish()
        .map_err(|source| PublishError::PackagingFailed {
            id: id.to_owned(),
            source,
        })?;

    Ok(out_path)
}

// ---------------------------------------------------------------------------
// Collection detection
// ---------------------------------------------------------------------------

/// Returns `true` when `target` is a single template (has `devcontainer-template.json`
/// directly inside it).  Returns `false` for a collection directory.
pub fn is_single_template(target: &Path) -> bool {
    target.join("devcontainer-template.json").exists()
}

/// Collect immediate child directories of `target` that contain
/// `devcontainer-template.json`.  Hidden directories (starting with `.`) are
/// skipped, matching the official CLI's behaviour.
fn collect_collection_dirs(target: &Path) -> Result<Vec<PathBuf>, PublishError> {
    let entries = std::fs::read_dir(target).map_err(|source| PublishError::Io {
        id: "<collection>".to_owned(),
        source,
    })?;

    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if name.starts_with('.') {
                return None;
            }
            if !p.is_dir() {
                return None;
            }
            if p.join("devcontainer-template.json").exists() {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    dirs.sort();
    Ok(dirs)
}

// ---------------------------------------------------------------------------
// Semver fan-out
// ---------------------------------------------------------------------------

/// Compute the set of tags to push for `version` given the already-published
/// `tags`.
///
/// Mirrors `getSemanticTags` from the official CLI:
/// - If `tags` already contains the exact version → return `None` (skip).
/// - Otherwise, push `version` itself, plus the major, major.minor, and
///   `"latest"` alias **when** the new version is the highest in that range.
///
/// Returns `None` when the version should be skipped entirely.
pub fn compute_semver_tags(version: &str, tags: &[String]) -> Option<Vec<String>> {
    // If exact version already published, skip.
    if tags.iter().any(|t| t == version) {
        return None;
    }

    let parsed = Version::parse(version).ok()?;

    let mut result = Vec::new();

    // Major alias: push if new version > current max satisfying `major.x.x`
    let major_str = parsed.major.to_string();
    let major_req = semver::VersionReq::parse(&format!("={}.x", parsed.major))
        .unwrap_or(semver::VersionReq::STAR);
    if is_new_highest(version, tags, &major_req) {
        result.push(major_str);
    }

    // Major.minor alias
    let minor_str = format!("{}.{}", parsed.major, parsed.minor);
    let minor_req = semver::VersionReq::parse(&format!("={}.{}.x", parsed.major, parsed.minor))
        .unwrap_or(semver::VersionReq::STAR);
    if is_new_highest(version, tags, &minor_req) {
        result.push(minor_str);
    }

    // Exact version is always included.
    result.push(version.to_owned());

    // Latest alias: push if new version > current max of all versions.
    let star_req = semver::VersionReq::STAR;
    if is_new_highest(version, tags, &star_req) {
        result.push("latest".to_owned());
    }

    Some(result)
}

/// Returns `true` if `new_version` is strictly greater than the maximum
/// version in `tags` that satisfies `req`, or if no such version exists.
fn is_new_highest(new_version: &str, tags: &[String], req: &semver::VersionReq) -> bool {
    let Ok(new) = Version::parse(new_version) else {
        return false;
    };

    let max_existing: Option<Version> = tags
        .iter()
        .filter_map(|t| Version::parse(t).ok())
        .filter(|v| req.matches(v))
        .max();

    max_existing.is_none_or(|existing| new > existing)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_single_template ──────────────────────────────────────────────────

    #[test]
    fn single_template_detected_when_json_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("devcontainer-template.json"), "{}").unwrap();
        assert!(is_single_template(tmp.path()));
    }

    #[test]
    fn collection_detected_when_json_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_single_template(tmp.path()));
    }

    // ── compute_semver_tags ─────────────────────────────────────────────────

    #[test]
    fn first_publish_gets_all_aliases() {
        let tags = compute_semver_tags("1.2.3", &[]).unwrap();
        // Must contain exact version + aliases
        assert!(tags.contains(&"1.2.3".to_owned()));
        assert!(tags.contains(&"1.2".to_owned()));
        assert!(tags.contains(&"1".to_owned()));
        assert!(tags.contains(&"latest".to_owned()));
    }

    #[test]
    fn already_published_exact_returns_none() {
        let existing = vec!["1.2.3".to_owned()];
        assert!(compute_semver_tags("1.2.3", &existing).is_none());
    }

    #[test]
    fn patch_bump_no_major_minor_latest_retag_when_old_is_higher() {
        // 1.2.5 is already published; publishing 1.2.3 should not retag 1.2 or latest
        let existing = vec![
            "1.2.5".to_owned(),
            "1.2".to_owned(),
            "1".to_owned(),
            "latest".to_owned(),
        ];
        let tags = compute_semver_tags("1.2.3", &existing).unwrap();
        assert!(tags.contains(&"1.2.3".to_owned()));
        assert!(!tags.contains(&"1.2".to_owned()), "should not retag 1.2");
        assert!(
            !tags.contains(&"latest".to_owned()),
            "should not retag latest"
        );
    }

    #[test]
    fn higher_patch_gets_aliases() {
        // 1.2.2 already published; 1.2.3 should get 1.2 and latest aliases
        let existing = vec!["1.2.2".to_owned()];
        let tags = compute_semver_tags("1.2.3", &existing).unwrap();
        assert!(tags.contains(&"1.2.3".to_owned()));
        assert!(tags.contains(&"1.2".to_owned()));
        assert!(tags.contains(&"1".to_owned()));
        assert!(tags.contains(&"latest".to_owned()));
    }

    #[test]
    fn new_minor_gets_major_and_latest_but_old_minor_alias_not_retagged() {
        // 1.3.0 is higher than existing 1.2.x — gets 1, 1.3, latest
        let existing = vec!["1.2.0".to_owned()];
        let tags = compute_semver_tags("1.3.0", &existing).unwrap();
        assert!(tags.contains(&"1.3.0".to_owned()));
        assert!(tags.contains(&"1.3".to_owned()));
        assert!(tags.contains(&"1".to_owned()));
        assert!(tags.contains(&"latest".to_owned()));
    }

    #[test]
    fn new_major_gets_full_set() {
        let existing = vec!["1.0.0".to_owned(), "1".to_owned(), "latest".to_owned()];
        let tags = compute_semver_tags("2.0.0", &existing).unwrap();
        assert!(tags.contains(&"2.0.0".to_owned()));
        assert!(tags.contains(&"2.0".to_owned()));
        assert!(tags.contains(&"2".to_owned()));
        assert!(tags.contains(&"latest".to_owned()));
    }

    // ── derive_template_type ────────────────────────────────────────────────

    #[test]
    fn detects_image_type() {
        let dc = serde_json::json!({"image": "ubuntu:22.04"});
        assert_eq!(derive_template_type(&dc).unwrap(), TemplateType::Image);
    }

    #[test]
    fn detects_dockerfile_type() {
        let dc = serde_json::json!({"dockerFile": "Dockerfile"});
        assert_eq!(derive_template_type(&dc).unwrap(), TemplateType::Dockerfile);
    }

    #[test]
    fn detects_dockerfile_build_type() {
        let dc = serde_json::json!({"build": {"dockerfile": "Dockerfile"}});
        assert_eq!(derive_template_type(&dc).unwrap(), TemplateType::Dockerfile);
    }

    #[test]
    fn detects_compose_type() {
        let dc = serde_json::json!({"dockerComposeFile": "docker-compose.yml"});
        assert_eq!(
            derive_template_type(&dc).unwrap(),
            TemplateType::DockerCompose
        );
    }

    #[test]
    fn unknown_type_returns_error() {
        let dc = serde_json::json!({"name": "test"});
        assert!(derive_template_type(&dc).is_err());
    }

    // ── extract_feature_ids ─────────────────────────────────────────────────

    #[test]
    fn extracts_feature_ids_strips_tag() {
        let dc = serde_json::json!({
            "features": {
                "ghcr.io/devcontainers/features/node:1": {},
                "ghcr.io/devcontainers/features/rust:latest": {}
            }
        });
        let mut ids = extract_feature_ids(&dc);
        ids.sort();
        assert!(ids.contains(&"ghcr.io/devcontainers/features/node".to_owned()));
        assert!(ids.contains(&"ghcr.io/devcontainers/features/rust".to_owned()));
    }

    #[test]
    fn no_features_returns_empty() {
        let dc = serde_json::json!({"image": "ubuntu"});
        assert!(extract_feature_ids(&dc).is_empty());
    }

    // ── collect_files ───────────────────────────────────────────────────────

    #[test]
    fn collects_files_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("devcontainer-template.json"), "{}").unwrap();
        let sub = tmp.path().join(".devcontainer");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("devcontainer.json"), "{}").unwrap();

        let files = collect_files(tmp.path());
        assert!(
            files
                .iter()
                .any(|f| f.contains("devcontainer-template.json"))
        );
        assert!(files.iter().any(|f| f.contains("devcontainer.json")));
    }

    // ── strip_jsonc_comments ────────────────────────────────────────────────

    #[test]
    fn strips_line_comments() {
        let input = r#"{ // a comment
"key": "value" }"#;
        let out = strip_jsonc_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn strips_block_comments() {
        let input = r#"{ /* block */ "key": "value" }"#;
        let out = strip_jsonc_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn preserves_url_slashes_in_strings() {
        let input = r#"{"url": "https://example.com/path"}"#;
        let out = strip_jsonc_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "https://example.com/path");
    }

    // ── find_devcontainer_json ──────────────────────────────────────────────

    #[test]
    fn finds_flat_devcontainer_json() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".devcontainer.json"), "{}").unwrap();
        assert!(find_devcontainer_json(tmp.path()).is_some());
    }

    #[test]
    fn finds_nested_devcontainer_json() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join(".devcontainer");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("devcontainer.json"), "{}").unwrap();
        assert!(find_devcontainer_json(tmp.path()).is_some());
    }

    #[test]
    fn returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_devcontainer_json(tmp.path()).is_none());
    }

    // ── package_template ───────────────────────────────────────────────────

    #[test]
    fn packages_template_to_tgz() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(
            src.join("devcontainer-template.json"),
            r#"{"id":"test","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(src.join("README.md"), "# Test").unwrap();

        let out_dir = tmp.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();

        let tgz = package_template("test", &src, &out_dir).unwrap();
        assert!(tgz.exists());
        assert!(
            tgz.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("devcontainer-template-test")
        );
        assert!(tgz.metadata().unwrap().len() > 0);
    }
}
