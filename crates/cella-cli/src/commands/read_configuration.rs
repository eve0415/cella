use std::collections::HashMap;
use std::path::PathBuf;

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
}

impl ReadConfigurationArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let has_container_target = !self.id_label.is_empty() || self.container_id.is_some();

        let config_path = self.override_config.as_deref().or(self.config.as_deref());

        let (mut resolved, cwd) = if has_container_target {
            let client = self.backend.resolve_client().await?;
            let target = ContainerTarget {
                container_id: self.container_id.clone(),
                container_name: None,
                id_labels: self.id_label.clone(),
                workspace_folder: self.workspace_folder.clone(),
            };
            let container = target.resolve(&*client, false).await?;

            let workspace_path = container
                .labels
                .get("dev.cella.workspace_path")
                .ok_or("container has no dev.cella.workspace_path label")?;
            let ws = PathBuf::from(workspace_path);
            let res = resolve::config(&ws, config_path)?;
            (res, ws)
        } else {
            let ws = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;
            let res = resolve::config(&ws, config_path)?;
            (res, ws)
        };

        if let Some(ref additional) = self.additional_features {
            super::features::resolve::merge_additional_features(&mut resolved.config, additional)?;
        }

        let device_type = if resolved.config.get("dockerComposeFile").is_some() {
            "dockerCompose"
        } else if resolved.config.get("build").is_some()
            || resolved.config.get("dockerFile").is_some()
        {
            "dockerfile"
        } else {
            "image"
        };

        let workspace_basename = cwd.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let workspace_folder = resolved
            .config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map_or_else(|| format!("/workspaces/{workspace_basename}"), String::from);

        let config_path = resolved
            .config_path
            .canonicalize()
            .unwrap_or_else(|_| resolved.config_path.clone());
        let config_path_str = config_path.to_string_lossy();

        let mut output = json!({
            "configFilePath": {
                "fsPath": config_path_str,
                "$mid": 1,
                "path": config_path_str,
                "scheme": "file"
            },
            "configuration": resolved.config,
            "workspace": {
                "workspaceFolder": workspace_folder,
                "deviceContainerType": device_type
            }
        });

        if self.include_merged_configuration {
            output["mergedConfiguration"] =
                resolve_merged_config(&resolved.config, &resolved.config_path).await?;
        }

        println!("{}", serde_json::to_string_pretty(&output)?);
        Ok(())
    }
}

/// Build the `mergedConfiguration` output for a resolved devcontainer,
/// matching the official `mergeConfiguration` shape from the devcontainers CLI.
///
/// Reused by `up --include-merged-configuration`.
pub async fn resolve_merged_config(
    config: &serde_json::Value,
    config_path: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let features = super::features::resolve::extract_features(config);
    if features.is_empty() {
        return Ok(build_merged_output(config, None));
    }

    let platform =
        cella_features::oci::detect_platform(std::env::consts::OS, std::env::consts::ARCH);
    let cache = cella_features::cache::FeatureCache::new();
    let base_image = config.get("image").and_then(|v| v.as_str()).unwrap_or("");
    let base_ctx = cella_features::BaseImageContext {
        base_image,
        image_user: "root",
        metadata: None,
        // read-configuration only merges config; no label is persisted, so the
        // omit-remote-env flag does not apply here.
        omit_remote_env: false,
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

    Ok(build_merged_output(config, Some(&rf)))
}

/// Assemble the official `mergeConfiguration` output shape.
///
/// Official shape (from devcontainers/cli `src/spec-node/imageMetadata.ts`):
/// - Removes singular lifecycle keys; emits plural `*Commands` arrays (always,
///   even empty), raw command values verbatim, base config first then features.
/// - Removes `entrypoint`; emits `entrypoints` string array.
/// - `shutdownAction`: last-wins (devcontainer.json wins over features).
/// - `customizations`: `Record<tool, contributions[]>` — each source's
///   customization object pushed per tool key. Omitted when empty.
/// - `capAdd`, `securityOpt`, `forwardPorts`: Set union/dedup.
/// - `mounts`: concat then dedup by target (last per target wins).
/// - `init`, `privileged`: OR (any true wins).
/// - `containerEnv`, `remoteEnv`, `portsAttributes`: last-wins maps.
/// - Last-wins scalars: `remoteUser`, `containerUser`, `userEnvProbe`,
///   `overrideCommand`, `updateRemoteUserUID`, `waitFor`, `otherPortsAttributes`.
/// - Passthrough unchanged: everything else in the config not in `replaceProperties`.
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

    // --- entrypoints ---
    let entrypoints: Vec<_> = fc
        .map(|c| c.entrypoints.iter().map(|ep| json!(ep)).collect())
        .unwrap_or_default();
    merged["entrypoints"] = json!(entrypoints);

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
                },
            ],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: fc,
            metadata_label: String::new(),
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
        };

        let out = build_merged_output(&config, Some(&rf));

        assert_eq!(out["entrypoints"], json!(["/init-a.sh", "/init-b.sh"]));
        assert!(out.get("entrypoint").is_none());
    }

    #[test]
    fn merged_output_entrypoints_empty_when_no_features() {
        let config = json!({"image": "ubuntu"});
        let out = build_merged_output(&config, None);
        assert_eq!(out["entrypoints"], json!([]));
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
            }],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
            container_config: FeatureContainerConfig::default(),
            metadata_label: String::new(),
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
        let workspace = std::env::temp_dir().join(format!(
            "cella-read-cfg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
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
            additional_features: None,
        };
        // Capture stdout to verify the shape.
        // We just verify it doesn't error; shape tested via build_merged_output unit tests.
        args.execute().await.unwrap();
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
