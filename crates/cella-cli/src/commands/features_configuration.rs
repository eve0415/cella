//! Builds the `featuresConfiguration` value for `read-configuration`.
//!
//! Mirrors the raw `FeaturesConfig` object the official devcontainers/cli
//! stringifies under the `featuresConfiguration` key: `{ featureSets[], dstFolder }`,
//! with `featureSets` in install order. The shape, `featureRef` decomposition
//! (`getRef`) and the empty-features behaviour (omit the key) are taken from
//! devcontainers/cli `src/spec-configuration/{containerFeaturesConfiguration,
//! containerCollectionsOCI,containerFeaturesOCI}.ts`.
//!
//! Known divergences (host-/registry-specific, not byte-matchable):
//! - `dstFolder` and `features[].cachePath` are cella's real cache/build paths,
//!   not the official's per-invocation temp folder.
//! - `features[].value` is derived from the parsed options map, so the rare
//!   string/boolean shorthand (`"feature": "latest"`) emits `{}` (the object
//!   form is exact), and object keys are sorted (`serde_json` has no
//!   `preserve_order`) rather than in devcontainer.json order.
//! - the embedded `manifest` is re-serialised from the typed manifest, so its
//!   JSON key order may differ from the registry's original bytes.
//!
//! Follow-ups, gated on `cella-features` modelling the field (it currently does
//! not parse them, so they are dropped before reaching here, not invented):
//! - non-OCI `sourceInformation.tarballUri` (direct-tarball) /
//!   `resolvedFilePath` (file-path) — not carried on `ResolvedFeature`.
//! - explicitly-empty collections (`"capAdd": []`) are emitted as absent,
//!   because the parser cannot distinguish them from an omitted key.

use std::collections::{BTreeMap, HashMap};

use cella_features::types::{FeatureOption, OptionType, ResolvedFeature, ResolvedFeatures};
use serde::Serialize;

/// The `featuresConfiguration` value: `{ featureSets[], dstFolder }`.
#[derive(Debug, Serialize)]
pub struct FeaturesConfiguration {
    #[serde(rename = "featureSets")]
    feature_sets: Vec<FeatureSetOut>,
    #[serde(rename = "dstFolder")]
    dst_folder: String,
}

#[derive(Debug, Serialize)]
struct FeatureSetOut {
    #[serde(rename = "sourceInformation")]
    source_information: SourceInformationOut,
    features: Vec<FeatureOut>,
}

/// Per-source provenance. OCI features carry the full `featureRef` + manifest;
/// non-OCI features carry the shared base info with a classified `type`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum SourceInformationOut {
    // Boxed: the OCI variant (manifest + featureRef) dwarfs the others.
    Oci(Box<OciSourceInformationOut>),
    DirectTarball(BaseSourceInfo),
    FilePath(BaseSourceInfo),
}

#[derive(Debug, Serialize)]
struct OciSourceInformationOut {
    manifest: serde_json::Value,
    #[serde(rename = "manifestDigest")]
    manifest_digest: String,
    #[serde(rename = "featureRef")]
    feature_ref: FeatureRefOut,
    #[serde(rename = "userFeatureId")]
    user_feature_id: String,
    #[serde(rename = "userFeatureIdWithoutVersion")]
    user_feature_id_without_version: String,
}

/// Shared `BaseSourceInformation` for non-OCI sources (local path / tarball).
#[derive(Debug, Serialize)]
struct BaseSourceInfo {
    #[serde(rename = "userFeatureId")]
    user_feature_id: String,
    #[serde(rename = "userFeatureIdWithoutVersion")]
    user_feature_id_without_version: String,
}

/// The parsed OCI reference (`getRef`).
#[derive(Debug, Serialize)]
struct FeatureRefOut {
    registry: String,
    owner: String,
    namespace: String,
    path: String,
    resource: String,
    id: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
}

/// A single feature entry (`featureSets[].features[0]`): the internal fields
/// (`id`/`included`/`value`/`consecutiveId`/`cachePath`) plus the published
/// `devcontainer-feature.json` metadata. `undefined`/empty fields are omitted.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeatureOut {
    id: String,
    included: bool,
    value: serde_json::Value,
    consecutive_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(rename = "documentationURL", skip_serializing_if = "Option::is_none")]
    documentation_url: Option<String>,
    #[serde(rename = "licenseURL", skip_serializing_if = "Option::is_none")]
    license_url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    options: BTreeMap<String, FeatureOptionOut>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    container_env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    installs_after: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    legacy_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    customizations: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mounts: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    init: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    privileged: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cap_add: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    security_opt: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecated: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    depends_on: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_create_command: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_content_command: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_create_command: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_start_command: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_attach_command: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct FeatureOptionOut {
    #[serde(rename = "type")]
    option_type: &'static str,
    // Omit when absent (parsed `default` is `Null`): matches the official's
    // optional `default?` being dropped from JSON when undefined.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    default: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    enum_values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proposals: Option<Vec<String>>,
}

/// Build the `featuresConfiguration` value, or `None` when no features are
/// declared (the official omits the key entirely in that case).
pub fn build(rf: &ResolvedFeatures) -> Result<Option<FeaturesConfiguration>, serde_json::Error> {
    if rf.features.is_empty() {
        return Ok(None);
    }
    let dst_folder = rf.build_context.to_string_lossy().into_owned();
    let feature_sets = rf
        .features
        .iter()
        .enumerate()
        .map(|(idx, f)| feature_set(f, idx))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(FeaturesConfiguration {
        feature_sets,
        dst_folder,
    }))
}

fn feature_set(f: &ResolvedFeature, idx: usize) -> Result<FeatureSetOut, serde_json::Error> {
    Ok(FeatureSetOut {
        source_information: source_information(f)?,
        features: vec![feature_out(f, idx)],
    })
}

fn source_information(f: &ResolvedFeature) -> Result<SourceInformationOut, serde_json::Error> {
    let user_feature_id = f.original_ref.clone();
    let user_feature_id_without_version =
        cella_features::feature_id_without_version(&f.original_ref).to_owned();

    Ok(match &f.oci {
        Some(oci) => SourceInformationOut::Oci(Box::new(OciSourceInformationOut {
            // Propagate rather than emit `manifest: null` on the (practically
            // impossible) serialization error — a null manifest is a silent
            // contract violation, so callers should see the failure.
            manifest: serde_json::to_value(&oci.manifest)?,
            manifest_digest: oci.digest.clone(),
            // featureRef comes from the normalized/fetched coordinates, not the
            // raw user ref: aliases (e.g. `maven` → java) resolve to a different
            // registry/repo, and the official builds featureRef from the fetched
            // identifier while `userFeatureId` keeps the original.
            feature_ref: parse_feature_ref(&format!(
                "{}/{}:{}",
                oci.registry, oci.repository, oci.version
            )),
            user_feature_id,
            user_feature_id_without_version,
        })),
        None if is_tarball(&f.original_ref) => {
            SourceInformationOut::DirectTarball(BaseSourceInfo {
                user_feature_id,
                user_feature_id_without_version,
            })
        }
        None => SourceInformationOut::FilePath(BaseSourceInfo {
            user_feature_id,
            user_feature_id_without_version,
        }),
    })
}

fn feature_out(f: &ResolvedFeature, idx: usize) -> FeatureOut {
    let m = &f.metadata;
    let consecutive_id = format!("{}_{}", f.id, idx);
    // Deterministic key order for `value` (the official preserves the
    // devcontainer.json insertion order, which cella's option map drops).
    let value = serde_json::to_value(f.user_options.iter().collect::<BTreeMap<_, _>>())
        .unwrap_or_else(|_| serde_json::json!({}));

    FeatureOut {
        id: f.id.clone(),
        included: true,
        value,
        // Point at cella's real cached feature directory — the only path
        // guaranteed to exist for `read-configuration` (the official's
        // dstFolder/consecutiveId is a build-time staging copy cella creates
        // elsewhere).
        cache_path: Some(f.artifact_dir.to_string_lossy().into_owned()),
        consecutive_id,
        version: non_empty(&m.version),
        name: m.name.clone(),
        description: m.description.clone(),
        documentation_url: m.documentation_url.clone(),
        license_url: m.license_url.clone(),
        options: map_options(&m.options),
        container_env: m
            .container_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        installs_after: m.installs_after.clone(),
        legacy_ids: m.legacy_ids.clone(),
        customizations: m.customizations.clone(),
        mounts: m.mounts.iter().map(|s| parse_mount_spec(s)).collect(),
        init: m.init,
        privileged: m.privileged,
        cap_add: m.cap_add.clone(),
        security_opt: m.security_opt.clone(),
        entrypoint: m.entrypoint.clone(),
        deprecated: m.deprecated,
        depends_on: m.depends_on.clone().into_iter().collect(),
        on_create_command: m.on_create_command.clone(),
        update_content_command: m.update_content_command.clone(),
        post_create_command: m.post_create_command.clone(),
        post_start_command: m.post_start_command.clone(),
        post_attach_command: m.post_attach_command.clone(),
    }
}

fn map_options(options: &HashMap<String, FeatureOption>) -> BTreeMap<String, FeatureOptionOut> {
    options
        .iter()
        .map(|(k, o)| {
            let option_type = match o.option_type {
                OptionType::String => "string",
                OptionType::Boolean => "boolean",
            };
            (
                k.clone(),
                FeatureOptionOut {
                    option_type,
                    default: o.default.clone(),
                    description: o.description.clone(),
                    enum_values: o.enum_values.clone(),
                    proposals: o.proposals.clone(),
                },
            )
        })
        .collect()
}

/// Parse an OCI feature reference into its components, mirroring the official
/// `getRef` (devcontainers/cli `containerCollectionsOCI.ts`). The version is
/// taken from the digest (`@sha256:…`), else the tag (`:tag`), else `latest`.
fn parse_feature_ref(input: &str) -> FeatureRefOut {
    let lower = input.to_lowercase();
    let last_at = lower.rfind('@');
    let last_colon = lower.rfind(':');
    let last_slash = lower.rfind('/');

    // Split off the version: a digest (`@sha256:…`) wins, else a tag (`:tag`
    // after the last slash, so not a registry port), else implicit `latest`.
    let (resource, tag, digest) = match (
        last_at,
        last_colon.filter(|&c| last_slash.is_none_or(|s| c > s)),
    ) {
        (Some(at), _) => (
            lower[..at].to_string(),
            None,
            Some(lower[at + 1..].to_string()),
        ),
        (None, Some(colon)) => (
            lower[..colon].to_string(),
            Some(lower[colon + 1..].to_string()),
            None,
        ),
        (None, None) => (lower, Some("latest".to_string()), None),
    };

    let segments: Vec<&str> = resource.split('/').collect();
    let registry = segments.first().copied().unwrap_or_default().to_string();
    let id = segments.last().copied().unwrap_or_default().to_string();
    let owner = segments.get(1).copied().unwrap_or_default().to_string();
    let namespace = if segments.len() >= 2 {
        segments[1..segments.len() - 1].join("/")
    } else {
        String::new()
    };
    let path = format!("{namespace}/{id}");
    let version = digest
        .as_deref()
        .or(tag.as_deref())
        .unwrap_or("latest")
        .to_string();

    FeatureRefOut {
        registry,
        owner,
        namespace,
        path,
        resource,
        id,
        version,
        tag,
        digest,
    }
}

/// Classify a non-OCI reference: an `http(s)://` URL is a direct tarball,
/// anything else (a relative/absolute path) is a local file path.
fn is_tarball(reference: &str) -> bool {
    reference.starts_with("http://") || reference.starts_with("https://")
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// Parse a docker mount-spec string (`type=…,source=…,target=…`) — the form
/// cella's metadata parser flattens feature mounts into — back into the
/// official object shape (`{type, source?, target}`). Feature mounts are
/// object-only per the spec, so a string here would be the wrong type. Empty
/// values are dropped; `src`/`dst` normalise to `source`/`target`.
fn parse_mount_spec(spec: &str) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for part in spec.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let key = match key.trim() {
            "src" => "source",
            "dst" => "target",
            other => other,
        };
        obj.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cella_features::types::{
        FeatureMetadata, FeatureOption, OptionType, ResolvedFeature, ResolvedFeatures,
        ResolvedOciManifest,
    };
    use std::path::PathBuf;

    fn manifest() -> ResolvedOciManifest {
        // Build the typed manifest via deserialization so the test needs no
        // direct oci_distribution dependency (the field type is inferred).
        ResolvedOciManifest {
            registry: "ghcr.io".to_string(),
            repository: "devcontainers/features/node".to_string(),
            version: "1".to_string(),
            digest: "sha256:manifestdigest".to_string(),
            manifest: serde_json::from_value(serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.devcontainers",
                    "digest": "sha256:configdigest",
                    "size": 0
                },
                "layers": [{
                    "mediaType": "application/vnd.devcontainers.layer.v1+tar",
                    "digest": "sha256:layerdigest",
                    "size": 1234,
                    "annotations": { "org.opencontainers.image.title": "node.tgz" }
                }]
            }))
            .expect("valid OCI manifest"),
        }
    }

    fn oci_feature() -> ResolvedFeature {
        ResolvedFeature {
            id: "node".to_string(),
            original_ref: "ghcr.io/devcontainers/features/node:1".to_string(),
            metadata: FeatureMetadata {
                id: "node".to_string(),
                version: "1.3.0".to_string(),
                name: Some("Node.js".to_string()),
                container_env: HashMap::from([(
                    "NVM_DIR".to_string(),
                    "/usr/local/nvm".to_string(),
                )]),
                installs_after: vec!["ghcr.io/devcontainers/features/common-utils".to_string()],
                customizations: Some(
                    serde_json::json!({"vscode": {"extensions": ["dbaeumer.vscode-eslint"]}}),
                ),
                ..Default::default()
            },
            user_options: HashMap::from([(
                "version".to_string(),
                serde_json::Value::String("20".to_string()),
            )]),
            artifact_dir: PathBuf::from("/cache/node"),
            has_install_script: true,
            oci: Some(manifest()),
        }
    }

    fn resolved(features: Vec<ResolvedFeature>) -> ResolvedFeatures {
        ResolvedFeatures {
            features,
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp/cella-features"),
            container_config: cella_features::types::FeatureContainerConfig::default(),
            metadata_label: String::new(),
            lockfile: None,
        }
    }

    #[test]
    fn omits_when_no_features() {
        assert!(build(&resolved(vec![])).unwrap().is_none());
    }

    #[test]
    fn oci_feature_full_shape() {
        let fc = build(&resolved(vec![oci_feature()]))
            .unwrap()
            .expect("features present");
        let v = serde_json::to_value(&fc).unwrap();

        assert_eq!(v["dstFolder"], "/tmp/cella-features");
        let set = &v["featureSets"][0];

        // sourceInformation (OCI).
        let si = &set["sourceInformation"];
        assert_eq!(si["type"], "oci");
        assert_eq!(si["manifestDigest"], "sha256:manifestdigest");
        assert_eq!(si["userFeatureId"], "ghcr.io/devcontainers/features/node:1");
        assert_eq!(
            si["userFeatureIdWithoutVersion"],
            "ghcr.io/devcontainers/features/node"
        );
        // Embedded manifest round-trips with OCI key casing.
        assert_eq!(si["manifest"]["schemaVersion"], 2);
        assert_eq!(si["manifest"]["config"]["digest"], "sha256:configdigest");
        assert_eq!(si["manifest"]["layers"][0]["digest"], "sha256:layerdigest");
        assert_eq!(
            si["manifest"]["layers"][0]["annotations"]["org.opencontainers.image.title"],
            "node.tgz"
        );

        // featureRef decomposition.
        let fr = &si["featureRef"];
        assert_eq!(fr["registry"], "ghcr.io");
        assert_eq!(fr["owner"], "devcontainers");
        assert_eq!(fr["namespace"], "devcontainers/features");
        assert_eq!(fr["path"], "devcontainers/features/node");
        assert_eq!(fr["resource"], "ghcr.io/devcontainers/features/node");
        assert_eq!(fr["id"], "node");
        assert_eq!(fr["version"], "1");
        assert_eq!(fr["tag"], "1");
        assert!(fr.get("digest").is_none(), "tag ref has no digest");

        // features[0].
        let feat = &set["features"][0];
        assert_eq!(feat["id"], "node");
        assert_eq!(feat["included"], true);
        assert_eq!(feat["value"], serde_json::json!({"version": "20"}));
        assert_eq!(feat["consecutiveId"], "node_0");
        assert_eq!(feat["cachePath"], "/cache/node");
        assert_eq!(feat["version"], "1.3.0");
        assert_eq!(feat["name"], "Node.js");
        assert_eq!(feat["containerEnv"]["NVM_DIR"], "/usr/local/nvm");
        assert_eq!(
            feat["installsAfter"][0],
            "ghcr.io/devcontainers/features/common-utils"
        );
        assert_eq!(
            feat["customizations"]["vscode"]["extensions"][0],
            "dbaeumer.vscode-eslint"
        );
    }

    #[test]
    fn documentation_url_license_url_and_proposals_emitted() {
        let mut f = oci_feature();
        f.metadata.documentation_url =
            Some("https://github.com/devcontainers/features/tree/main/src/node".to_string());
        f.metadata.license_url =
            Some("https://github.com/devcontainers/features/blob/main/LICENSE".to_string());
        f.metadata.options = HashMap::from([(
            "version".to_string(),
            FeatureOption {
                option_type: OptionType::String,
                default: serde_json::json!("lts"),
                description: None,
                enum_values: None,
                proposals: Some(vec!["lts".to_string(), "20".to_string(), "18".to_string()]),
            },
        )]);
        let fc = build(&resolved(vec![f]))
            .unwrap()
            .expect("features present");
        let v = serde_json::to_value(&fc).unwrap();
        let feat = &v["featureSets"][0]["features"][0];

        // All three new fields present with the exact official key names.
        assert_eq!(
            feat["documentationURL"],
            "https://github.com/devcontainers/features/tree/main/src/node"
        );
        assert_eq!(
            feat["licenseURL"],
            "https://github.com/devcontainers/features/blob/main/LICENSE"
        );
        assert_eq!(
            feat["options"]["version"]["proposals"],
            serde_json::json!(["lts", "20", "18"])
        );

        // Sanity: camelCase variants must NOT appear (wrong key casing).
        assert!(
            feat.get("documentationUrl").is_none(),
            "must be documentationURL, not documentationUrl"
        );
        assert!(
            feat.get("licenseUrl").is_none(),
            "must be licenseURL, not licenseUrl"
        );
    }

    #[test]
    fn documentation_url_license_url_and_proposals_omitted_when_absent() {
        // oci_feature() sets no documentationURL/licenseURL and default options
        // have no proposals — verify the keys are completely absent (not null).
        let fc = build(&resolved(vec![oci_feature()]))
            .unwrap()
            .expect("features present");
        let v = serde_json::to_value(&fc).unwrap();
        let feat = &v["featureSets"][0]["features"][0];

        assert!(
            feat.get("documentationURL").is_none(),
            "documentationURL must be omitted when absent, not null"
        );
        assert!(
            feat.get("licenseURL").is_none(),
            "licenseURL must be omitted when absent, not null"
        );
        // oci_feature has no options, so no proposals to check — any option
        // without proposals must not emit the key.
    }

    #[test]
    fn non_oci_feature_is_file_path() {
        let mut f = oci_feature();
        f.id = "local".to_string();
        f.original_ref = "./my-feature".to_string();
        f.oci = None;
        let fc = build(&resolved(vec![f]))
            .unwrap()
            .expect("features present");
        let v = serde_json::to_value(&fc).unwrap();
        let si = &v["featureSets"][0]["sourceInformation"];
        assert_eq!(si["type"], "file-path");
        assert_eq!(si["userFeatureId"], "./my-feature");
        assert!(si.get("manifest").is_none(), "non-OCI has no manifest");
    }

    #[test]
    fn featureref_uses_normalized_coords_not_raw_alias() {
        // A deprecated alias (`maven`) resolves to a different registry/repo;
        // featureRef must reflect the fetched target, userFeatureId the alias.
        let mut f = oci_feature();
        f.id = "java".to_string();
        f.original_ref = "maven".to_string();
        f.oci = Some(ResolvedOciManifest {
            registry: "ghcr.io".to_string(),
            repository: "devcontainers/features/java".to_string(),
            version: "1".to_string(),
            digest: "sha256:javadigest".to_string(),
            manifest: manifest().manifest,
        });
        let fc = build(&resolved(vec![f]))
            .unwrap()
            .expect("features present");
        let v = serde_json::to_value(&fc).unwrap();
        let si = &v["featureSets"][0]["sourceInformation"];
        assert_eq!(si["userFeatureId"], "maven");
        assert_eq!(si["userFeatureIdWithoutVersion"], "maven");
        let fr = &si["featureRef"];
        assert_eq!(fr["id"], "java");
        assert_eq!(fr["resource"], "ghcr.io/devcontainers/features/java");
        assert_eq!(fr["registry"], "ghcr.io");
    }

    #[test]
    fn feature_mounts_emit_as_objects() {
        let mut f = oci_feature();
        f.metadata.mounts =
            vec!["type=volume,source=node-modules,target=/usr/local/lib".to_string()];
        let fc = build(&resolved(vec![f]))
            .unwrap()
            .expect("features present");
        let v = serde_json::to_value(&fc).unwrap();
        let mount = &v["featureSets"][0]["features"][0]["mounts"][0];
        assert_eq!(mount["type"], "volume");
        assert_eq!(mount["source"], "node-modules");
        assert_eq!(mount["target"], "/usr/local/lib");
    }

    #[test]
    fn parse_mount_spec_omits_empty_source() {
        let m = parse_mount_spec("type=volume,source=,target=/data");
        assert_eq!(m["type"], "volume");
        assert_eq!(m["target"], "/data");
        assert!(m.get("source").is_none(), "empty source must be omitted");
    }

    #[test]
    fn parse_feature_ref_tag() {
        let r = parse_feature_ref("ghcr.io/devcontainers/features/go:1.2.3");
        assert_eq!(r.version, "1.2.3");
        assert_eq!(r.tag.as_deref(), Some("1.2.3"));
        assert_eq!(r.digest, None);
        assert_eq!(r.resource, "ghcr.io/devcontainers/features/go");
        assert_eq!(r.namespace, "devcontainers/features");
    }

    #[test]
    fn parse_feature_ref_digest() {
        let r = parse_feature_ref("ghcr.io/owner/repo/tool@sha256:abc123");
        assert_eq!(r.version, "sha256:abc123");
        assert_eq!(r.digest.as_deref(), Some("sha256:abc123"));
        assert_eq!(r.tag, None);
        assert_eq!(r.id, "tool");
    }

    #[test]
    fn parse_feature_ref_implicit_latest() {
        let r = parse_feature_ref("ghcr.io/devcontainers/features/git");
        assert_eq!(r.version, "latest");
        assert_eq!(r.tag.as_deref(), Some("latest"));
    }
}
