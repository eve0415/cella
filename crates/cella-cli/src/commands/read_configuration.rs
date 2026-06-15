use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;

use cella_backend::ContainerTarget;
use cella_config::devcontainer::resolve;
use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};

use crate::backend::BackendArgs;

/// Read and output the resolved devcontainer configuration.
#[derive(Args)]
pub struct ReadConfigurationArgs {
    #[command(flatten)]
    backend: BackendArgs,

    /// Workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Include merged configuration from features.
    #[arg(long)]
    include_merged_configuration: bool,

    /// Include features configuration details.
    #[arg(long)]
    include_features_configuration: bool,

    /// Path to a devcontainer.json that replaces the discovered config entirely.
    #[arg(long = "override-config")]
    override_config: Option<PathBuf>,

    /// Target container by label(s) of the form `name=value` (repeatable).
    #[arg(long = "id-label", value_parser = crate::commands::parse_id_label)]
    id_label: Vec<String>,

    /// Target container by ID.
    #[arg(long = "container-id")]
    container_id: Option<String>,

    /// Additional features as JSON string.
    #[arg(long = "additional-features")]
    additional_features: Option<String>,

    /// Shared devcontainer-CLI compat flags (`--docker-path`,
    /// `--user-data-folder`, `--mount-*`, `--log-level`/`--log-format`,
    /// `--terminal-*`, `--skip-feature-auto-mapping`). All no-ops except
    /// log-level/log-format (seeded into tracing by `main.rs`). The
    /// `--mount-workspace-git-root` value is not consumed here: cella always
    /// reports the git-root mount (see `find_git_root_folder` below), matching
    /// the official default of `true`.
    #[command(flatten)]
    pub(crate) compat: super::WorkspaceCompatArgs,
}

impl ReadConfigurationArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let has_container_target = !self.id_label.is_empty() || self.container_id.is_some();

        let config_path = self.override_config.as_deref().or(self.config.as_deref());

        // When targeting a running container, capture its `devcontainer.metadata` label so
        // `mergedConfiguration` can be built from the already-baked label (no OCI access),
        // matching the official CLI's behaviour.
        let (mut resolved, cwd, container_metadata_label) = if has_container_target {
            let client = self.backend.resolve_client().await?;
            let target = ContainerTarget {
                container_id: self.container_id.clone(),
                container_name: None,
                id_labels: self.id_label.clone(),
                workspace_folder: self.workspace_folder.clone(),
            };
            let container = target.resolve(&*client, false).await?;

            let metadata_label = container
                .labels
                .get("devcontainer.metadata")
                .cloned()
                .unwrap_or_default();

            // The official `getImageMetadataFromContainer` treats a container
            // found by `--id-label` specially: only the current config's
            // updateable props are merged (the baked lifecycle stays). True when
            // id-labels were given and all are present on the container.
            let has_id_labels = !self.id_label.is_empty()
                && self.id_label.iter().all(|kv| {
                    kv.split_once('=').is_some_and(|(k, v)| {
                        container.labels.get(k).map(String::as_str) == Some(v)
                    })
                });

            let workspace_path = container
                .labels
                .get("dev.cella.workspace_path")
                .ok_or("container has no dev.cella.workspace_path label")?;
            let ws = PathBuf::from(workspace_path);
            let res = resolve::config(&ws, config_path)?;
            (res, ws, Some((metadata_label, has_id_labels)))
        } else {
            let ws = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let res = resolve::config(&ws, config_path)?;
            (res, ws, None)
        };

        if let Some(ref additional) = self.additional_features {
            super::features::resolve::merge_additional_features(&mut resolved.config, additional)?;
        }

        // Report the default single-container mount `up` would create: it
        // bind-mounts the git-root folder (mountWorkspaceGitRoot defaults to
        // true). read-configuration exposes no flag to override this, so the
        // default is reported. Compose configs ignore this (no bind mount).
        let host_mount_folder = cella_git::find_git_root_folder(&cwd, true);
        let mut output = build_base_output(
            &resolved.config,
            &resolved.config_path,
            &cwd,
            &host_mount_folder,
        );

        // `featuresConfiguration` is emitted when explicitly requested, or when
        // `mergedConfiguration` is requested without a container target — the
        // official `needsFeaturesConfig = includeFeaturesConfig || (includeMergedConfig
        // && !container)`. It always derives from the config's features (never a
        // container label), so resolve once and share with the merged path.
        let needs_features_config = self.include_features_configuration
            || (self.include_merged_configuration && !has_container_target);
        let resolved_features = if needs_features_config {
            resolve_config_features(&resolved.config, &resolved.config_path).await?
        } else {
            None
        };

        if self.include_merged_configuration {
            output["mergedConfiguration"] =
                if let Some((label, has_id_labels)) = container_metadata_label {
                    // Container target: read from the already-baked `devcontainer.metadata`
                    // label — no OCI network access needed. Matches the official CLI's
                    // `getImageMetadataFromContainer` path.
                    build_merged_from_label(&resolved.config, &label, has_id_labels)
                } else {
                    build_merged_output(&resolved.config, resolved_features.as_ref())
                };
        }

        if needs_features_config
            && let Some(rf) = resolved_features.as_ref()
            && let Some(fc) = super::features_configuration::build(rf)?
        {
            output["featuresConfiguration"] = serde_json::to_value(fc)?;
        }

        // Official `read-configuration` prints compact (single-line) JSON.
        println!("{}", serde_json::to_string(&output)?);
        Ok(())
    }
}

/// Build the base `read-configuration` envelope: `{configuration, workspace}`.
///
/// Matches the official shape: `configFilePath` is nested *inside*
/// `configuration` (not a top-level sibling), and `workspace` is the official
/// `WorkspaceConfiguration` (`workspaceFolder` plus, for single-container
/// configs, `workspaceMount`), not the cella-only `deviceContainerType`.
///
/// `host_mount_folder` is the folder the caller resolved as cella's bind-mount
/// source (the git root by default); it may be a parent of `workspace_root`
/// when the workspace is a git subdirectory. It is unused for Compose configs,
/// which own their workspace volumes in the service definition.
fn build_base_output(
    config: &serde_json::Value,
    config_path: &Path,
    workspace_root: &Path,
    host_mount_folder: &Path,
) -> serde_json::Value {
    let mut configuration = config.clone();
    super::up::inject_config_file_path(&mut configuration, config_path);

    let workspace = if config.get("dockerComposeFile").is_some() {
        // Compose: the service definition owns the workspace volume, so cella
        // (like the official CLI) creates no single-container bind mount —
        // `workspaceMount` is omitted. `workspaceFolder` defaults to "/".
        let workspace_folder = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();
        json!({ "workspaceFolder": workspace_folder })
    } else {
        // The container mount point mirrors the host mount folder's name; the
        // workspace folder may sit in a subdirectory beneath it.
        let mount_basename = host_mount_folder.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let container_mount_folder = format!("/workspaces/{mount_basename}");
        let workspace_folder = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map_or_else(
                || super::up::compute_default_workspace_folder(workspace_root, host_mount_folder),
                String::from,
            );

        // An explicit `workspaceMount` in the config (including `""`) is
        // reported verbatim — official keys on the property's presence —
        // otherwise derive the default git-root bind string.
        let workspace_mount = config.get("workspaceMount").cloned().unwrap_or_else(|| {
            json!(derive_workspace_mount(
                host_mount_folder,
                &container_mount_folder
            ))
        });
        json!({
            "workspaceFolder": workspace_folder,
            "workspaceMount": workspace_mount,
        })
    };

    json!({
        "configuration": configuration,
        "workspace": workspace,
    })
}

/// Derive the default workspace bind-mount string for `host_mount_folder` (the
/// folder cella mounts), matching `cella_config`'s `map_workspace_mount`:
/// source = the canonicalized host folder, target = the container mount point,
/// `consistency` only off Linux.
fn derive_workspace_mount(host_mount_folder: &Path, container_target: &str) -> String {
    let canonical = host_mount_folder
        .canonicalize()
        .unwrap_or_else(|_| host_mount_folder.to_path_buf());
    let source = canonical.to_string_lossy();
    let cons = if cfg!(target_os = "linux") {
        ""
    } else {
        ",consistency=cached"
    };
    // Quote source/target when they contain a comma, matching the official
    // mount-string formatting.
    let src_q = if source.contains(',') { "\"" } else { "" };
    let tgt_q = if container_target.contains(',') {
        "\""
    } else {
        ""
    };
    format!("type=bind,{src_q}source={source}{src_q},{tgt_q}target={container_target}{tgt_q}{cons}")
}

/// Build the `mergedConfiguration` output for a resolved devcontainer,
/// matching the official `mergeConfiguration` shape from the devcontainers CLI.
///
/// Reused by `up --include-merged-configuration`.
pub async fn resolve_merged_config(
    config: &serde_json::Value,
    config_path: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let rf = resolve_config_features(config, config_path).await?;
    Ok(build_merged_output(config, rf.as_ref()))
}

/// Resolve the config's features for `mergedConfiguration`/`featuresConfiguration`,
/// or `None` when the config declares no features.
///
/// Always derives from the config (never a container label), so it can be shared
/// between both outputs in a single `read-configuration` invocation.
pub async fn resolve_config_features(
    config: &serde_json::Value,
    config_path: &Path,
) -> Result<Option<ResolvedFeatures>, Box<dyn std::error::Error + Send + Sync>> {
    let features = super::features::resolve::extract_features(config);
    if features.is_empty() {
        return Ok(None);
    }

    let platform =
        cella_features::oci::detect_platform(std::env::consts::OS, std::env::consts::ARCH);
    let cache = cella_features::cache::FeatureCache::new();
    let base_image = config.get("image").and_then(|v| v.as_str()).unwrap_or("");
    let base_ctx = cella_features::BaseImageContext {
        base_image,
        image_user: "root",
        metadata: None,
        // read-configuration only merges config; no label is persisted.
        omit: cella_features::MetadataOmit::default(),
    };
    let rf = cella_features::resolve_features(
        config,
        config_path,
        &platform,
        &cache,
        &base_ctx,
        false,
        cella_features::LockfilePolicy::NoLockfile,
    )
    .await?;

    Ok(Some(rf))
}

/// Build `mergedConfiguration` from a container's baked `devcontainer.metadata`
/// label, with no OCI/network access — mirroring the official
/// `getImageMetadataFromContainer` + `mergeConfiguration`.
///
/// The official appends the *current* devcontainer.json to the baked metadata
/// array so it is authoritative in the merge: the `--id-label` path contributes
/// only the updateable properties (`remoteUser`/`userEnvProbe`/`remoteEnv`),
/// while other targets (e.g. `--container-id`) append the full config (incl
/// lifecycle/customizations). A single-object label is normalised to one entry.
fn build_merged_from_label(
    config: &serde_json::Value,
    label_json: &str,
    has_id_labels: bool,
) -> serde_json::Value {
    use cella_features::types::{FeatureMetadata, ResolvedFeature};
    use std::collections::HashMap;
    use std::path::PathBuf;

    // The baked metadata is normally a JSON array; some tooling/images emit a
    // single object instead — normalise it to a one-element array so its
    // metadata is not silently dropped.
    let mut entries: Vec<serde_json::Value> =
        match serde_json::from_str::<serde_json::Value>(label_json) {
            Ok(serde_json::Value::Array(a)) => a,
            Ok(obj @ serde_json::Value::Object(_)) => vec![obj],
            _ => Vec::new(),
        };

    // Append the current config last so it is authoritative (matches the official
    // `getImageMetadataFromContainer`). `--id-label` contributes only updateable
    // props; otherwise the full config (its lifecycle/customizations win).
    entries.push(if has_id_labels {
        pick_properties(config, &["remoteUser", "userEnvProbe", "remoteEnv"])
    } else {
        config.clone()
    });

    // Merge the entries into a FeatureContainerConfig (lifecycle/env/caps/mounts),
    // ordered base→features→devcontainer.json, with no network access.
    let array_json = serde_json::to_string(&entries).unwrap_or_default();
    let (container_config, _user_info) = cella_features::parse_image_metadata(&array_json);

    // Reconstruct per-feature entries (those with an "id") for the per-source
    // customizations Record; devcontainer.json entries (no "id") are handled by
    // `build_merged_output` via `config` directly.
    let features: Vec<ResolvedFeature> = entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(|v| v.as_str())?.to_string();
            let customizations = entry.get("customizations").cloned();
            Some(ResolvedFeature {
                id: id.clone(),
                original_ref: id.clone(),
                metadata: FeatureMetadata {
                    id,
                    customizations,
                    ..Default::default()
                },
                user_options: HashMap::new(),
                artifact_dir: PathBuf::new(),
                has_install_script: false,
                oci: None,
            })
        })
        .collect();

    let rf = ResolvedFeatures {
        features,
        dockerfile: String::new(),
        build_context: PathBuf::new(),
        container_config,
        metadata_label: String::new(),
        lockfile: None,
    };

    build_merged_output(config, Some(&rf))
}

/// Pick a subset of object keys into a new JSON object (empty when none present).
fn pick_properties(config: &serde_json::Value, keys: &[&str]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(obj) = config.as_object() {
        for &key in keys {
            if let Some(value) = obj.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    serde_json::Value::Object(out)
}

/// Assemble the official `mergeConfiguration` output shape.
///
/// Official shape (from devcontainers/cli `src/spec-node/imageMetadata.ts`):
/// - Removes singular lifecycle keys; emits plural `*Commands` arrays (always,
///   even empty), raw command values verbatim, features in install order then
///   devcontainer.json last.
/// - Removes `entrypoint`; emits `entrypoints` string array. Omitted when empty.
/// - `shutdownAction`: last-wins (devcontainer.json wins over features).
/// - `customizations`: `Record<tool, contributions[]>` — each source's
///   customization object pushed per tool key. Omitted when empty.
/// - `capAdd`, `securityOpt`: Set union/dedup.
/// - `mounts`: concat then dedup by target (last per target wins).
/// - `init`, `privileged`: OR (any true wins).
/// - `containerEnv`: feature contributions merged over base (feature wins
///   collisions, matching metadata-array last-wins from features before
///   devcontainer.json; omitted when no feature contributions).
/// - Passthrough unchanged: everything else in the config not in `replaceProperties`.
///   This includes `forwardPorts`, `remoteEnv`, `portsAttributes`, `remoteUser`,
///   and other last-wins scalars — they pass through from the devcontainer.json
///   config unchanged (no cross-source merge needed for the read-configuration path).
fn build_merged_output(
    config: &serde_json::Value,
    rf: Option<&ResolvedFeatures>,
) -> serde_json::Value {
    // Start from config, removing the keys that are replaced in the merged shape.
    let replace_keys = [
        "customizations",
        "entrypoint",
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
        "shutdownAction",
    ];
    let mut merged = config.clone();
    if let Some(obj) = merged.as_object_mut() {
        for key in &replace_keys {
            obj.remove(*key);
        }
    }

    let fc = rf.map(|r| &r.container_config);

    // --- Lifecycle plural arrays (ALWAYS emitted, even empty) ---
    // Order: features in install order, then the devcontainer.json command last.
    merged["onCreateCommands"] = lifecycle_commands_array(config, "onCreateCommand", fc);
    merged["updateContentCommands"] = lifecycle_commands_array(config, "updateContentCommand", fc);
    merged["postCreateCommands"] = lifecycle_commands_array(config, "postCreateCommand", fc);
    merged["postStartCommands"] = lifecycle_commands_array(config, "postStartCommand", fc);
    merged["postAttachCommands"] = lifecycle_commands_array(config, "postAttachCommand", fc);

    // --- entrypoints: omit when empty, matching official behaviour ---
    let entrypoints: Vec<serde_json::Value> = fc
        .map(|c| c.entrypoints.iter().map(|ep| json!(ep)).collect())
        .unwrap_or_default();
    if entrypoints.is_empty() {
        if let Some(obj) = merged.as_object_mut() {
            obj.remove("entrypoints");
        }
    } else {
        merged["entrypoints"] = json!(entrypoints);
    }

    // --- shutdownAction last-wins (devcontainer.json wins) ---
    // Features don't carry shutdownAction; only devcontainer.json does.
    if let Some(sa) = config.get("shutdownAction").cloned() {
        merged["shutdownAction"] = sa;
    }

    // --- mounts: dedup by target, last per target wins ---
    apply_merged_mounts(&mut merged, config, fc);

    // --- init / privileged: OR ---
    let user_init = config
        .get("init")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let user_privileged = config
        .get("privileged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    merged["init"] = json!(fc.is_some_and(|c| c.init) || user_init);
    merged["privileged"] = json!(fc.is_some_and(|c| c.privileged) || user_privileged);

    // --- capAdd / securityOpt: Set union/dedup ---
    apply_merged_string_set(
        &mut merged,
        config,
        "capAdd",
        fc.map(|c| c.cap_add.as_slice()),
    );
    apply_merged_string_set(
        &mut merged,
        config,
        "securityOpt",
        fc.map(|c| c.security_opt.as_slice()),
    );

    // --- containerEnv: feature contributions merged over the base (last wins) ---
    if let Some(fc) = fc
        && !fc.container_env.is_empty()
    {
        let mut env_map = config
            .get("containerEnv")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (k, v) in &fc.container_env {
            env_map.insert(k.clone(), json!(v));
        }
        merged["containerEnv"] = serde_json::Value::Object(env_map);
    }

    // --- customizations: Record<tool, contributions[]> ---
    apply_merged_customizations(&mut merged, config, rf);

    merged
}

/// Build a lifecycle plural-array value from base config + feature entries.
///
/// The official `mergeConfiguration` collects raw command values verbatim from
/// the image-metadata array `[base-image…, features in install order,
/// devcontainer.json]` — so the devcontainer.json command is collected LAST.
/// `merge_with_devcontainer` already builds `fc.lifecycle.*` in exactly that
/// order (devcontainer.json appended last), so we emit those entries verbatim.
/// With no features, the devcontainer.json command is the sole element.
fn lifecycle_commands_array(
    config: &serde_json::Value,
    singular_key: &str,
    fc: Option<&FeatureContainerConfig>,
) -> serde_json::Value {
    let mut commands: Vec<serde_json::Value> = Vec::new();

    if let Some(fc) = fc {
        let feature_entries = match singular_key {
            "onCreateCommand" => &fc.lifecycle.on_create,
            "updateContentCommand" => &fc.lifecycle.update_content,
            "postCreateCommand" => &fc.lifecycle.post_create,
            "postStartCommand" => &fc.lifecycle.post_start,
            "postAttachCommand" => &fc.lifecycle.post_attach,
            _ => return json!(commands),
        };
        // Already ordered features-then-devcontainer.json by the merge step.
        for entry in feature_entries {
            commands.push(entry.command.clone());
        }
    } else if let Some(cmd) = config.get(singular_key).filter(|v| !v.is_null()) {
        // No features resolved: the devcontainer.json command is the only entry.
        commands.push(cmd.clone());
    }

    json!(commands)
}

/// Apply mounts with dedup-by-target semantics (last per target wins).
fn apply_merged_mounts(
    merged: &mut serde_json::Value,
    config: &serde_json::Value,
    fc: Option<&FeatureContainerConfig>,
) {
    // Collect all mounts: feature mounts first (install order), then user config.
    let mut all: Vec<serde_json::Value> = Vec::new();
    if let Some(fc) = fc {
        for m in &fc.mounts {
            all.push(json!(m));
        }
    }
    if let Some(user_mounts) = config.get("mounts").and_then(|v| v.as_array()) {
        all.extend(user_mounts.iter().cloned());
    }

    if all.is_empty() {
        // Omit mounts entirely when empty, matching official behaviour.
        if let Some(obj) = merged.as_object_mut() {
            obj.remove("mounts");
        }
        return;
    }

    // Dedup by target — last per target wins (iterate reversed, keep first seen).
    let mut seen_targets: std::collections::HashSet<String> = std::collections::HashSet::new();
    let deduped: Vec<_> = all
        .into_iter()
        .rev()
        .filter(|m| {
            let target = extract_mount_target(m);
            target.is_none_or(|t| seen_targets.insert(t))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    merged["mounts"] = json!(deduped);
}

/// Extract the `target` field from a mount (string CSV or object form).
fn extract_mount_target(mount: &serde_json::Value) -> Option<String> {
    if let Some(s) = mount.as_str() {
        // CSV form: "type=bind,source=/src,target=/dst"
        for part in s.split(',') {
            if let Some(val) = part
                .strip_prefix("target=")
                .or_else(|| part.strip_prefix("dst="))
            {
                return Some(val.to_string());
            }
        }
        None
    } else if let Some(obj) = mount.as_object() {
        obj.get("target")
            .or_else(|| obj.get("dst"))
            .and_then(|v| v.as_str())
            .map(String::from)
    } else {
        None
    }
}

/// Apply a string-set union for `capAdd` or `securityOpt`.
fn apply_merged_string_set(
    merged: &mut serde_json::Value,
    config: &serde_json::Value,
    key: &str,
    feature_vals: Option<&[String]>,
) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result: Vec<serde_json::Value> = Vec::new();

    let push_dedup = |item: &str,
                      seen: &mut std::collections::HashSet<String>,
                      result: &mut Vec<serde_json::Value>| {
        if seen.insert(item.to_string()) {
            result.push(json!(item));
        }
    };

    if let Some(vals) = feature_vals {
        for v in vals {
            push_dedup(v, &mut seen, &mut result);
        }
    }
    if let Some(arr) = config.get(key).and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                push_dedup(s, &mut seen, &mut result);
            }
        }
    }

    if result.is_empty() {
        if let Some(obj) = merged.as_object_mut() {
            obj.remove(key);
        }
    } else {
        merged[key] = json!(result);
    }
}

/// Build `customizations` as `Record<tool, contributions[]>`.
///
/// Each source (feature or devcontainer.json) that has a customization object
/// contributes its raw value for each tool key as one element of that key's
/// array. Order: features in install order, then devcontainer.json.
///
/// If no customizations exist, the key is omitted.
///
/// **Limitation**: `FeatureContainerConfig.customizations` is a deep-merged
/// single blob, not a per-source breakdown. To build faithful per-source arrays
/// we need per-feature data, which is available via `ResolvedFeatures.features`.
/// We use that directly for feature contributions. The devcontainer.json
/// contribution is taken verbatim from `config["customizations"]`.
fn apply_merged_customizations(
    merged: &mut serde_json::Value,
    config: &serde_json::Value,
    rf: Option<&ResolvedFeatures>,
) {
    let mut per_tool: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    // Feature contributions in install order.
    if let Some(rf) = rf {
        for feature in &rf.features {
            if let Some(obj) = feature
                .metadata
                .customizations
                .as_ref()
                .and_then(serde_json::Value::as_object)
            {
                for (tool, val) in obj {
                    per_tool.entry(tool.clone()).or_default().push(val.clone());
                }
            }
        }
    }

    // devcontainer.json contribution last.
    if let Some(obj) = config
        .get("customizations")
        .and_then(serde_json::Value::as_object)
    {
        for (tool, val) in obj {
            per_tool.entry(tool.clone()).or_default().push(val.clone());
        }
    }

    if per_tool.is_empty() {
        if let Some(obj) = merged.as_object_mut() {
            obj.remove("customizations");
        }
    } else {
        let map: serde_json::Map<String, serde_json::Value> =
            per_tool.into_iter().map(|(k, v)| (k, json!(v))).collect();
        merged["customizations"] = serde_json::Value::Object(map);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    #[test]
    fn id_label_is_repeatable_and_validated() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ReadConfigurationArgs,
        }

        // Repeatable: two --id-label flags collect into the Vec.
        let cli = Cli::try_parse_from([
            "read-configuration",
            "--id-label",
            "a=1",
            "--id-label",
            "b=2",
        ])
        .expect("two id-labels must parse");
        assert_eq!(
            cli.args.id_label,
            vec!["a=1".to_string(), "b=2".to_string()]
        );

        // Validated: a malformed label (no `=`) is rejected.
        assert!(
            Cli::try_parse_from(["read-configuration", "--id-label", "noequals"]).is_err(),
            "malformed --id-label must be rejected"
        );
    }

    // ── devcontainer-CLI flag parity ───────────────────────────────
    //
    // Source of truth: devcontainers/cli `src/spec-node/devContainersSpecCLI.ts`
    // `readConfigurationOptions` (lines 993-1012). Every official long flag MUST
    // be declared so no official `devcontainer read-configuration` invocation
    // errors with "unknown argument". This test exists because the command
    // drifted from the spec for lack of one.
    const OFFICIAL_READ_CONFIGURATION_FLAGS: &[&str] = &[
        "user-data-folder",
        "docker-path",
        "docker-compose-path",
        "workspace-folder",
        "mount-workspace-git-root",
        "mount-git-worktree-common-dir",
        "container-id",
        "id-label",
        "config",
        "override-config",
        "log-level",
        "log-format",
        "terminal-columns",
        "terminal-rows",
        "include-features-configuration",
        "include-merged-configuration",
        "additional-features",
        "skip-feature-auto-mapping",
    ];

    #[test]
    fn read_configuration_flag_parity() {
        use clap::CommandFactory;
        use std::collections::HashSet;
        let cli = crate::Cli::command();
        let cmd = cli
            .find_subcommand("read-configuration")
            .expect("`read-configuration` subcommand must exist");
        let longs: HashSet<&str> = cmd
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect();
        let missing: Vec<&&str> = OFFICIAL_READ_CONFIGURATION_FLAGS
            .iter()
            .filter(|f| !longs.contains(**f))
            .collect();
        assert!(
            missing.is_empty(),
            "`read-configuration` is missing official flags: {missing:?}"
        );
    }

    #[test]
    fn read_configuration_accepts_all_new_compat_flags() {
        use clap::Parser;
        // A maximal official-style invocation must parse Ok.
        let cli = crate::Cli::try_parse_from([
            "cella",
            "read-configuration",
            "--log-format",
            "json",
            "--log-level",
            "debug",
            "--user-data-folder",
            "/x",
            "--docker-path",
            "/usr/bin/docker",
            "--docker-compose-path",
            "/usr/bin/docker-compose",
            "--mount-workspace-git-root",
            "false",
            "--mount-git-worktree-common-dir",
            "--terminal-columns",
            "80",
            "--terminal-rows",
            "40",
            "--skip-feature-auto-mapping",
        ])
        .expect("all official read-configuration compat flags must parse");
        let crate::commands::Command::ReadConfiguration(args) = &cli.command else {
            panic!("expected read-configuration subcommand");
        };
        assert!(matches!(
            args.compat.log_level,
            Some(super::super::LogLevel::Debug)
        ));
        assert!(matches!(
            args.compat.log_format,
            super::super::LogFormat::Json
        ));
        assert!(!args.compat.mount_workspace_git_root);
    }

    #[test]
    fn read_configuration_terminal_columns_requires_rows() {
        use clap::Parser;
        let r =
            crate::Cli::try_parse_from(["cella", "read-configuration", "--terminal-columns", "80"]);
        assert!(r.is_err(), "--terminal-columns alone must be rejected");
    }

    // -------------------------------------------------------------------------
    // build_merged_output — lifecycle plural arrays
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_lifecycle_plural_always_emitted_base_only() {
        let config = json!({
            "image": "ubuntu:22.04",
            "onCreateCommand": "echo create",
            "postCreateCommand": ["apt-get", "install", "-y", "curl"],
        });
        let out = build_merged_output(&config, None);

        // Plural arrays present
        assert!(out.get("onCreateCommands").is_some());
        assert!(out.get("postCreateCommands").is_some());
        assert!(out.get("updateContentCommands").is_some());
        assert!(out.get("postStartCommands").is_some());
        assert!(out.get("postAttachCommands").is_some());

        // Singular keys absent
        assert!(out.get("onCreateCommand").is_none());
        assert!(out.get("postCreateCommand").is_none());

        // Values are raw command, wrapped in array
        assert_eq!(out["onCreateCommands"], json!(["echo create"]));
        assert_eq!(
            out["postCreateCommands"],
            json!([["apt-get", "install", "-y", "curl"]])
        );
        // Empty phases are empty arrays
        assert_eq!(out["updateContentCommands"], json!([]));
        assert_eq!(out["postStartCommands"], json!([]));
        assert_eq!(out["postAttachCommands"], json!([]));
    }

    #[test]
    fn merged_output_lifecycle_plural_empty_when_no_commands() {
        let config = json!({"image": "ubuntu:22.04"});
        let out = build_merged_output(&config, None);

        assert_eq!(out["onCreateCommands"], json!([]));
        assert_eq!(out["updateContentCommands"], json!([]));
        assert_eq!(out["postCreateCommands"], json!([]));
        assert_eq!(out["postStartCommands"], json!([]));
        assert_eq!(out["postAttachCommands"], json!([]));
    }

    #[test]
    fn merged_output_lifecycle_features_then_devcontainer_last() {
        use cella_features::types::{
            FeatureContainerConfig, FeatureLifecycle, LifecycleEntry, ResolvedFeature,
        };
        use std::collections::HashMap;
        use std::path::PathBuf;

        let config = json!({
            "image": "ubuntu",
            "onCreateCommand": "echo base",
        });

        // Simulate two features each contributing onCreateCommand
        let fc = FeatureContainerConfig {
            lifecycle: FeatureLifecycle {
                // As merge_with_devcontainer builds it: features in install
                // order, then the devcontainer.json entry appended LAST.
                on_create: vec![
                    LifecycleEntry {
                        origin: "feat-a".to_string(),
                        command: json!("echo feat-a"),
                    },
                    LifecycleEntry {
                        origin: "feat-b".to_string(),
                        command: json!("echo feat-b"),
                    },
                    LifecycleEntry {
                        origin: "devcontainer.json".to_string(),
                        command: json!("echo base"),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let rf = ResolvedFeatures {
            features: vec![
                ResolvedFeature {
                    id: "feat-a".to_string(),
                    original_ref: "feat-a".to_string(),
                    metadata: cella_features::types::FeatureMetadata {
                        id: "feat-a".to_string(),
                        on_create_command: Some(json!("echo feat-a")),
                        ..Default::default()
                    },
                    user_options: HashMap::new(),
                    artifact_dir: PathBuf::from("/tmp"),
                    has_install_script: false,
                    oci: None,
                },
                ResolvedFeature {
                    id: "feat-b".to_string(),
                    original_ref: "feat-b".to_string(),
                    metadata: cella_features::types::FeatureMetadata {
                        id: "feat-b".to_string(),
                        on_create_command: Some(json!("echo feat-b")),
                        ..Default::default()
                    },
                    user_options: HashMap::new(),
                    artifact_dir: PathBuf::from("/tmp"),
                    has_install_script: false,
                    oci: None,
                },
            ],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };

        let out = build_merged_output(&config, Some(&rf));

        // Features in install order, then the devcontainer.json command LAST
        // (matches official mergeConfiguration's metadata-array order).
        assert_eq!(
            out["onCreateCommands"],
            json!(["echo feat-a", "echo feat-b", "echo base"])
        );
        assert!(out.get("onCreateCommand").is_none());
    }

    #[test]
    fn merged_output_singular_lifecycle_keys_absent() {
        let config = json!({
            "image": "ubuntu",
            "onCreateCommand": "setup",
            "updateContentCommand": "update",
            "postCreateCommand": "post-create",
            "postStartCommand": "post-start",
            "postAttachCommand": "post-attach",
        });
        let out = build_merged_output(&config, None);

        // All singular keys must be gone
        for key in &[
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ] {
            assert!(
                out.get(key).is_none(),
                "singular key {key} should be absent from merged output"
            );
        }
        // Plural versions must be present
        assert_eq!(out["onCreateCommands"], json!(["setup"]));
        assert_eq!(out["updateContentCommands"], json!(["update"]));
        assert_eq!(out["postCreateCommands"], json!(["post-create"]));
        assert_eq!(out["postStartCommands"], json!(["post-start"]));
        assert_eq!(out["postAttachCommands"], json!(["post-attach"]));
    }

    // -------------------------------------------------------------------------
    // build_merged_output — entrypoints
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_entrypoints_from_features() {
        use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};

        let config = json!({"image": "ubuntu"});
        let fc = FeatureContainerConfig {
            entrypoints: vec!["/init-a.sh".to_string(), "/init-b.sh".to_string()],
            ..Default::default()
        };
        let rf = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };

        let out = build_merged_output(&config, Some(&rf));

        assert_eq!(out["entrypoints"], json!(["/init-a.sh", "/init-b.sh"]));
        assert!(out.get("entrypoint").is_none());
    }

    #[test]
    fn merged_output_entrypoints_omitted_when_empty() {
        let config = json!({"image": "ubuntu"});
        let out = build_merged_output(&config, None);
        // Official mergeConfiguration omits entrypoints when there are none.
        assert!(
            out.get("entrypoints").is_none(),
            "entrypoints should be absent when empty"
        );
    }

    // -------------------------------------------------------------------------
    // build_merged_output — shutdownAction last-wins
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_shutdown_action_from_devcontainer() {
        let config = json!({"image": "ubuntu", "shutdownAction": "stopContainer"});
        let out = build_merged_output(&config, None);
        assert_eq!(out["shutdownAction"], json!("stopContainer"));
    }

    #[test]
    fn merged_output_shutdown_action_absent_when_not_set() {
        let config = json!({"image": "ubuntu"});
        let out = build_merged_output(&config, None);
        assert!(out.get("shutdownAction").is_none());
    }

    // -------------------------------------------------------------------------
    // build_merged_output — mounts dedup by target
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_mounts_dedup_by_target_last_wins() {
        use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};

        let config = json!({
            "image": "ubuntu",
            // User config mount at /data — same target as feature, user wins (last)
            "mounts": ["type=bind,source=/host2,target=/data"],
        });

        let fc = FeatureContainerConfig {
            // Feature provides a mount at /data too
            mounts: vec!["type=volume,source=vol1,target=/data".to_string()],
            ..Default::default()
        };
        let rf = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };

        let out = build_merged_output(&config, Some(&rf));
        let mounts = out["mounts"].as_array().unwrap();

        // Only one mount at /data — the last one (user config) wins
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0], json!("type=bind,source=/host2,target=/data"));
    }

    #[test]
    fn merged_output_mounts_different_targets_both_kept() {
        use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};

        let config = json!({
            "image": "ubuntu",
            "mounts": ["type=bind,source=/host,target=/workspace"],
        });
        let fc = FeatureContainerConfig {
            mounts: vec!["type=volume,source=cache,target=/cache".to_string()],
            ..Default::default()
        };
        let rf = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };

        let out = build_merged_output(&config, Some(&rf));
        let mounts = out["mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn merged_output_mounts_absent_when_empty() {
        let config = json!({"image": "ubuntu"});
        let out = build_merged_output(&config, None);
        assert!(out.get("mounts").is_none());
    }

    // -------------------------------------------------------------------------
    // build_merged_output — init / privileged OR
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_container_env_merges_feature_over_base() {
        use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};
        use std::collections::HashMap;

        let config =
            json!({ "image": "ubuntu", "containerEnv": { "BASE": "1", "SHARED": "base" } });
        let fc = FeatureContainerConfig {
            container_env: HashMap::from([
                ("FEAT".to_string(), "2".to_string()),
                ("SHARED".to_string(), "feature".to_string()),
            ]),
            ..Default::default()
        };
        let rf = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };
        let out = build_merged_output(&config, Some(&rf));
        // Base-only key survives, feature key added, feature wins the collision.
        assert_eq!(out["containerEnv"]["BASE"], json!("1"));
        assert_eq!(out["containerEnv"]["FEAT"], json!("2"));
        assert_eq!(out["containerEnv"]["SHARED"], json!("feature"));
    }

    #[test]
    fn merged_output_init_or_semantics() {
        use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};

        // Feature true, user absent => true
        let fc = FeatureContainerConfig {
            init: true,
            ..Default::default()
        };
        let rf = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };
        let out = build_merged_output(&json!({"image": "ubuntu"}), Some(&rf));
        assert_eq!(out["init"], json!(true));

        // Feature false, user true => true
        let fc2 = FeatureContainerConfig {
            init: false,
            ..Default::default()
        };
        let rf2 = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc2,
            metadata_label: String::new(),
            lockfile: None,
        };
        let out2 = build_merged_output(&json!({"image": "ubuntu", "init": true}), Some(&rf2));
        assert_eq!(out2["init"], json!(true));

        // Both false => false
        let out3 = build_merged_output(&json!({"image": "ubuntu", "init": false}), None);
        assert_eq!(out3["init"], json!(false));
    }

    #[test]
    fn merged_output_privileged_or_semantics() {
        use cella_features::types::{FeatureContainerConfig, ResolvedFeatures};

        let fc = FeatureContainerConfig {
            privileged: true,
            ..Default::default()
        };
        let rf = ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
            lockfile: None,
        };
        let out = build_merged_output(&json!({"image": "ubuntu"}), Some(&rf));
        assert_eq!(out["privileged"], json!(true));
    }

    // -------------------------------------------------------------------------
    // build_merged_output — customizations Record<tool, contributions[]>
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_customizations_record_shape() {
        use cella_features::types::{
            FeatureContainerConfig, FeatureMetadata, ResolvedFeature, ResolvedFeatures,
        };
        use std::collections::HashMap;

        let config = json!({
            "image": "ubuntu",
            "customizations": {
                "vscode": {"settings": {"editor.fontSize": 16}}
            }
        });

        let rf = ResolvedFeatures {
            features: vec![ResolvedFeature {
                id: "feat-a".to_string(),
                original_ref: "feat-a".to_string(),
                metadata: FeatureMetadata {
                    id: "feat-a".to_string(),
                    customizations: Some(json!({
                        "vscode": {"extensions": ["ext-a"]}
                    })),
                    ..Default::default()
                },
                user_options: HashMap::new(),
                artifact_dir: PathBuf::from("/tmp"),
                has_install_script: false,
                oci: None,
            }],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: FeatureContainerConfig::default(),
            metadata_label: String::new(),
            lockfile: None,
        };

        let out = build_merged_output(&config, Some(&rf));
        let cust = &out["customizations"];

        // vscode has two contributions: feature first, then devcontainer.json
        let vscode_arr = cust["vscode"].as_array().unwrap();
        assert_eq!(vscode_arr.len(), 2);
        assert_eq!(vscode_arr[0], json!({"extensions": ["ext-a"]}));
        assert_eq!(vscode_arr[1], json!({"settings": {"editor.fontSize": 16}}));
    }

    #[test]
    fn merged_output_customizations_omitted_when_empty() {
        let config = json!({"image": "ubuntu"});
        let out = build_merged_output(&config, None);
        assert!(out.get("customizations").is_none());
    }

    // -------------------------------------------------------------------------
    // build_merged_output — passthrough keys
    // -------------------------------------------------------------------------

    #[test]
    fn merged_output_passthrough_image_and_name() {
        let config = json!({
            "image": "ubuntu:22.04",
            "name": "my-container",
            "remoteUser": "vscode",
            "workspaceFolder": "/workspace",
        });
        let out = build_merged_output(&config, None);
        assert_eq!(out["image"], json!("ubuntu:22.04"));
        assert_eq!(out["name"], json!("my-container"));
        assert_eq!(out["remoteUser"], json!("vscode"));
        assert_eq!(out["workspaceFolder"], json!("/workspace"));
    }

    // -------------------------------------------------------------------------
    // extract_mount_target
    // -------------------------------------------------------------------------

    #[test]
    fn extract_mount_target_csv_target_key() {
        let m = json!("type=bind,source=/src,target=/dst");
        assert_eq!(extract_mount_target(&m), Some("/dst".to_string()));
    }

    #[test]
    fn extract_mount_target_csv_dst_key() {
        let m = json!("type=volume,src=vol,dst=/data");
        assert_eq!(extract_mount_target(&m), Some("/data".to_string()));
    }

    #[test]
    fn extract_mount_target_object_form() {
        let m = json!({"type": "bind", "source": "/src", "target": "/dst"});
        assert_eq!(extract_mount_target(&m), Some("/dst".to_string()));
    }

    #[test]
    fn extract_mount_target_missing_returns_none() {
        let m = json!("type=bind,source=/src");
        assert_eq!(extract_mount_target(&m), None);
    }

    // -------------------------------------------------------------------------
    // Integration: execute command
    // -------------------------------------------------------------------------

    fn make_temp_workspace(config_content: &str) -> PathBuf {
        // Process id + a monotonic counter guarantees a unique path per call,
        // even across parallel tests. A nanosecond timestamp alone can collide
        // on CI VMs with coarse clock resolution, letting one test's
        // `remove_dir_all` delete another's workspace mid-run (flaky failures).
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let workspace = std::env::temp_dir().join(format!(
            "cella-read-cfg-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let dc_dir = workspace.join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).expect("create .devcontainer dir");
        std::fs::write(dc_dir.join("devcontainer.json"), config_content)
            .expect("write devcontainer.json");
        workspace
    }

    #[tokio::test]
    async fn execute_basic_read_succeeds() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            compat: crate::commands::WorkspaceCompatArgs::default(),
            additional_features: None,
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_override_config_uses_override_file() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let override_path = workspace.join("override.json");
        std::fs::write(&override_path, r#"{"image": "alpine:3.18"}"#).unwrap();

        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: Some(override_path),
            id_label: Vec::new(),
            container_id: None,
            compat: crate::commands::WorkspaceCompatArgs::default(),
            additional_features: None,
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_additional_features_valid_json() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            compat: crate::commands::WorkspaceCompatArgs::default(),
            additional_features: Some(
                r#"{"ghcr.io/devcontainers/features/git:1": {}}"#.to_string(),
            ),
        };
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_additional_features_invalid_json_returns_error() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            compat: crate::commands::WorkspaceCompatArgs::default(),
            additional_features: Some("not valid json".to_string()),
        };
        let err = args.execute().await.unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_additional_features_non_object_returns_error() {
        let workspace = make_temp_workspace(r#"{"image": "ubuntu:22.04"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: false,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            compat: crate::commands::WorkspaceCompatArgs::default(),
            additional_features: Some(r#"["not", "an", "object"]"#.to_string()),
        };
        let err = args.execute().await.unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_include_merged_configuration_no_features_emits_plural_keys() {
        let workspace =
            make_temp_workspace(r#"{"image": "ubuntu:22.04", "postCreateCommand": "echo hi"}"#);
        let args = ReadConfigurationArgs {
            backend: BackendArgs::default(),
            workspace_folder: Some(workspace.clone()),
            config: None,
            include_merged_configuration: true,
            include_features_configuration: false,
            override_config: None,
            id_label: Vec::new(),
            container_id: None,
            compat: crate::commands::WorkspaceCompatArgs::default(),
            additional_features: None,
        };
        // Capture stdout to verify the shape.
        // We just verify it doesn't error; shape tested via build_merged_output unit tests.
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }

    // --- read-configuration envelope shape (build_base_output) ---

    // Common case: workspace == git root, so host_mount_folder == workspace_root.
    #[test]
    fn base_output_nests_config_file_path_not_top_level() {
        let config = json!({"image": "ubuntu:22.04"});
        let out = build_base_output(
            &config,
            Path::new("/work/proj/.devcontainer/devcontainer.json"),
            Path::new("/work/proj"),
            Path::new("/work/proj"),
        );
        assert!(
            out.get("configFilePath").is_none(),
            "configFilePath must not be a top-level sibling"
        );
        assert_eq!(
            out["configuration"]["configFilePath"]["scheme"],
            json!("file")
        );
        assert_eq!(out["configuration"]["image"], json!("ubuntu:22.04"));
    }

    #[test]
    fn base_output_workspace_shape_drops_device_container_type() {
        let config = json!({"image": "ubuntu:22.04"});
        let out = build_base_output(
            &config,
            Path::new("/work/proj/.devcontainer/devcontainer.json"),
            Path::new("/work/proj"),
            Path::new("/work/proj"),
        );
        let ws = &out["workspace"];
        assert!(
            ws.get("deviceContainerType").is_none(),
            "deviceContainerType is a cella-only phantom key"
        );
        assert_eq!(ws["workspaceFolder"], json!("/workspaces/proj"));
        let mount = ws["workspaceMount"].as_str().unwrap();
        assert!(mount.starts_with("type=bind,"), "got: {mount}");
        assert!(mount.contains("target=/workspaces/proj"), "got: {mount}");
    }

    // Git subdirectory: cella mounts the git root, so the report must too —
    // source = git root, target = the git-root mount point, workspaceFolder
    // sits in the subdir beneath it.
    #[test]
    fn base_output_git_subdir_reports_git_root_mount() {
        let config = json!({"image": "ubuntu"});
        let out = build_base_output(
            &config,
            Path::new("/repo/packages/app/.devcontainer/devcontainer.json"),
            Path::new("/repo/packages/app"),
            Path::new("/repo"),
        );
        let ws = &out["workspace"];
        assert_eq!(
            ws["workspaceFolder"],
            json!("/workspaces/repo/packages/app")
        );
        let mount = ws["workspaceMount"].as_str().unwrap();
        assert!(mount.contains("target=/workspaces/repo"), "got: {mount}");
        assert!(mount.contains("source=/repo"), "got: {mount}");
    }

    // Compose: the service owns its volumes, so no single-container workspace
    // bind mount is reported (matches official `getWorkspaceConfiguration`).
    #[test]
    fn base_output_compose_omits_workspace_mount() {
        let config = json!({
            "dockerComposeFile": "docker-compose.yml",
            "service": "app",
            "workspaceFolder": "/workspace"
        });
        let out = build_base_output(
            &config,
            Path::new("/repo/.devcontainer/devcontainer.json"),
            Path::new("/repo"),
            Path::new("/repo"),
        );
        let ws = &out["workspace"];
        assert_eq!(ws["workspaceFolder"], json!("/workspace"));
        assert!(
            ws.get("workspaceMount").is_none(),
            "compose configs have no single-container workspace bind mount"
        );
    }

    #[test]
    fn base_output_compose_workspace_folder_defaults_to_root() {
        let config = json!({"dockerComposeFile": "docker-compose.yml", "service": "app"});
        let out = build_base_output(
            &config,
            Path::new("/repo/.devcontainer/devcontainer.json"),
            Path::new("/repo"),
            Path::new("/repo"),
        );
        assert_eq!(out["workspace"]["workspaceFolder"], json!("/"));
        assert!(out["workspace"].get("workspaceMount").is_none());
    }

    #[test]
    fn base_output_explicit_empty_workspace_mount_verbatim() {
        // Presence-keyed: an explicit "" is reported as-is, not derived.
        let config = json!({"image": "ubuntu", "workspaceMount": ""});
        let out = build_base_output(
            &config,
            Path::new("/work/p/.devcontainer/devcontainer.json"),
            Path::new("/work/p"),
            Path::new("/work/p"),
        );
        assert_eq!(out["workspace"]["workspaceMount"], json!(""));
    }

    #[test]
    fn base_output_explicit_null_workspace_mount_verbatim() {
        let config = json!({"image": "ubuntu", "workspaceMount": null});
        let out = build_base_output(
            &config,
            Path::new("/work/p/.devcontainer/devcontainer.json"),
            Path::new("/work/p"),
            Path::new("/work/p"),
        );
        assert_eq!(out["workspace"]["workspaceMount"], json!(null));
    }

    #[test]
    fn base_output_explicit_custom_workspace_mount_verbatim() {
        let config = json!({"image": "ubuntu", "workspaceMount": "type=bind,source=/a,target=/b"});
        let out = build_base_output(
            &config,
            Path::new("/work/p/.devcontainer/devcontainer.json"),
            Path::new("/work/p"),
            Path::new("/work/p"),
        );
        assert_eq!(
            out["workspace"]["workspaceMount"],
            json!("type=bind,source=/a,target=/b")
        );
    }

    #[test]
    fn base_output_respects_explicit_workspace_folder() {
        let config = json!({"image": "ubuntu", "workspaceFolder": "/custom/wf"});
        let out = build_base_output(
            &config,
            Path::new("/work/p/.devcontainer/devcontainer.json"),
            Path::new("/work/p"),
            Path::new("/work/p"),
        );
        assert_eq!(out["workspace"]["workspaceFolder"], json!("/custom/wf"));
    }

    #[test]
    fn base_output_serializes_compact_single_line() {
        let config = json!({"image": "ubuntu"});
        let out = build_base_output(
            &config,
            Path::new("/work/p/.devcontainer/devcontainer.json"),
            Path::new("/work/p"),
            Path::new("/work/p"),
        );
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains('\n'), "compact output must be single-line");
    }

    // -------------------------------------------------------------------------
    // build_merged_from_label — container-path mergedConfiguration (no OCI)
    // -------------------------------------------------------------------------

    #[test]
    fn merged_from_label_appends_current_config_lifecycle_last() {
        // The label carries a feature's hook; the current devcontainer.json is
        // appended last (matching the official `getImageMetadataFromContainer`),
        // so its hook is authoritative and ordered after the feature's.
        let label = json!([{"id": "feat-a", "postCreateCommand": "echo feat-a"}]).to_string();
        let config = json!({"image": "ubuntu", "postCreateCommand": "echo dev"});
        let out = build_merged_from_label(&config, &label, false);

        assert_eq!(
            out["postCreateCommands"],
            json!(["echo feat-a", "echo dev"])
        );
        assert!(out.get("postCreateCommand").is_none());
    }

    #[test]
    fn merged_from_label_customizations_per_source_record_shape() {
        // The metadata label has two feature entries, each with customizations.
        let label = json!([
            {"id": "feat-a", "customizations": {"vscode": {"extensions": ["ext-a"]}}},
            {"id": "feat-b", "customizations": {"vscode": {"extensions": ["ext-b"]}}},
            {"image": "ubuntu"}  // devcontainer.json entry (no id)
        ])
        .to_string();
        // The on-disk config adds its own vscode customization.
        let config = json!({
            "image": "ubuntu",
            "customizations": {
                "vscode": {"settings": {"editor.fontSize": 14}}
            }
        });

        let out = build_merged_from_label(&config, &label, false);
        let cust = &out["customizations"];

        // vscode: feat-a contribution, feat-b contribution, devcontainer.json contribution.
        let vscode = cust["vscode"].as_array().expect("vscode must be an array");
        assert_eq!(vscode.len(), 3);
        assert_eq!(vscode[0], json!({"extensions": ["ext-a"]}));
        assert_eq!(vscode[1], json!({"extensions": ["ext-b"]}));
        assert_eq!(vscode[2], json!({"settings": {"editor.fontSize": 14}}));
    }

    #[test]
    fn merged_from_label_container_env_merged_from_label() {
        let label = json!([
            {"id": "feat-a", "containerEnv": {"FOO": "feat", "SHARED": "feat"}},
            {"image": "ubuntu", "containerEnv": {"SHARED": "base"}}
        ])
        .to_string();
        let config = json!({"image": "ubuntu", "containerEnv": {"BASE": "1"}});

        let out = build_merged_from_label(&config, &label, false);
        // All env vars from the label are merged into the output.
        assert_eq!(out["containerEnv"]["FOO"], json!("feat"));
        // Later entry in the label wins (last-wins semantics from parse_image_metadata).
        assert_eq!(out["containerEnv"]["SHARED"], json!("base"));
    }

    #[test]
    fn merged_from_label_empty_label_still_includes_current_config() {
        // A container with no devcontainer.metadata label (e.g. one built by other
        // tooling). Regression: the current devcontainer.json is still appended, so
        // its hooks are NOT dropped — the container path must merge the live config,
        // not ignore it.
        let config = json!({
            "image": "ubuntu",
            "postCreateCommand": "echo hi",
        });
        let out = build_merged_from_label(&config, "", false);

        // The current config's hook is present (appended as the sole entry).
        assert_eq!(out["postCreateCommands"], json!(["echo hi"]));
        assert!(out.get("postCreateCommand").is_none());
        assert_eq!(out["init"], json!(false));
        assert_eq!(out["privileged"], json!(false));
    }

    #[test]
    fn merged_from_label_absent_label_yields_empty_array_keys() {
        // Absent label (empty string from unwrap_or_default in execute).
        let config = json!({"image": "ubuntu"});
        let out = build_merged_from_label(&config, "", false);
        // All plural lifecycle arrays present and empty.
        assert_eq!(out["onCreateCommands"], json!([]));
        assert_eq!(out["updateContentCommands"], json!([]));
        assert_eq!(out["postCreateCommands"], json!([]));
        assert_eq!(out["postStartCommands"], json!([]));
        assert_eq!(out["postAttachCommands"], json!([]));
        // Customizations absent.
        assert!(out.get("customizations").is_none());
    }

    #[test]
    fn merged_from_label_init_privileged_or_from_label() {
        let label = json!([
            {"id": "feat-a", "init": true, "privileged": false},
            {"image": "ubuntu"}
        ])
        .to_string();
        let config = json!({"image": "ubuntu"});
        let out = build_merged_from_label(&config, &label, false);
        assert_eq!(out["init"], json!(true));
        assert_eq!(out["privileged"], json!(false));
    }

    #[test]
    fn merged_from_label_cap_add_dedup() {
        let label = json!([
            {"id": "feat-a", "capAdd": ["SYS_PTRACE"]},
            {"id": "feat-b", "capAdd": ["SYS_PTRACE", "NET_ADMIN"]},
            {"image": "ubuntu"}
        ])
        .to_string();
        let config = json!({"image": "ubuntu"});
        let out = build_merged_from_label(&config, &label, false);
        let cap_add = out["capAdd"].as_array().expect("capAdd must be present");
        // Deduplicated: SYS_PTRACE appears once.
        assert!(cap_add.contains(&json!("SYS_PTRACE")));
        assert!(cap_add.contains(&json!("NET_ADMIN")));
        assert_eq!(cap_add.len(), 2);
    }

    #[test]
    fn merged_from_label_entrypoints_from_label() {
        let label = json!([
            {"id": "feat-a", "entrypoint": "/init-feat.sh"},
            {"image": "ubuntu"}
        ])
        .to_string();
        let config = json!({"image": "ubuntu"});
        let out = build_merged_from_label(&config, &label, false);
        assert_eq!(out["entrypoints"], json!(["/init-feat.sh"]));
    }

    #[test]
    fn merged_from_label_passthrough_fields_preserved() {
        let label = json!([{"image": "ubuntu:22.04"}]).to_string();
        let config = json!({
            "image": "ubuntu:22.04",
            "remoteUser": "vscode",
            "workspaceFolder": "/workspace",
            "name": "my-dev",
        });
        let out = build_merged_from_label(&config, &label, false);
        assert_eq!(out["remoteUser"], json!("vscode"));
        assert_eq!(out["workspaceFolder"], json!("/workspace"));
        assert_eq!(out["name"], json!("my-dev"));
    }

    #[test]
    fn merged_from_label_normalizes_single_object_label() {
        // Some tooling/images emit `devcontainer.metadata` as a single object
        // rather than an array; it must be normalised to one entry, not dropped.
        let label = json!({"id": "feat-a", "postCreateCommand": "echo obj"}).to_string();
        let config = json!({"image": "ubuntu"});
        let out = build_merged_from_label(&config, &label, false);

        assert_eq!(out["postCreateCommands"], json!(["echo obj"]));
    }

    #[test]
    fn merged_from_label_id_label_path_omits_current_config_lifecycle() {
        // Found by `--id-label`: the official appends only the updateable props
        // (remoteUser/userEnvProbe/remoteEnv), NOT the current config's lifecycle,
        // so the baked/feature hooks stand.
        let label = json!([{"id": "feat-a", "postCreateCommand": "echo feat-a"}]).to_string();
        let config = json!({"postCreateCommand": "echo dev", "remoteUser": "vscode"});
        let out = build_merged_from_label(&config, &label, true);

        // The current config's lifecycle is NOT appended on the id-label path.
        assert_eq!(out["postCreateCommands"], json!(["echo feat-a"]));
    }
}
