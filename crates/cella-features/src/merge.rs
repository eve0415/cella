//! Merging feature metadata into a unified container configuration.
//!
//! Implements two merge phases from the devcontainer spec:
//!
//! 1. **Feature-to-feature**: accumulates metadata from all resolved features
//!    in install order into a single [`FeatureContainerConfig`].
//! 2. **Feature-to-devcontainer**: merges the accumulated feature config with
//!    the user's `devcontainer.json` settings, respecting user-overrides.

use std::collections::{HashMap, HashSet};

use crate::error::FeatureWarning;
use crate::types::{
    FeatureContainerConfig, FeatureLifecycle, FeatureOption, OptionType, ResolvedFeature,
};

/// Merge metadata from all resolved features into a single container config.
///
/// Features must be provided in install order. The merge rules follow the
/// devcontainer spec:
///
/// - `mounts`: concatenate in install order
/// - `capAdd`, `securityOpt`: concatenate and deduplicate
/// - `privileged`, `init`: OR (any `true` wins)
/// - `containerEnv`: merge maps; later features override earlier for same key
/// - `entrypoint`: collect into ordered list
/// - lifecycle commands: collect in install order
/// - `customizations`: deep merge in install order
pub fn merge_features(features: &[ResolvedFeature]) -> FeatureContainerConfig {
    let mut config = FeatureContainerConfig::default();

    for feature in features {
        let meta = &feature.metadata;

        // Mounts: concatenate in order.
        config.mounts.extend(meta.mounts.iter().cloned());

        // capAdd: concatenate, deduplicate.
        extend_dedup(&mut config.cap_add, &meta.cap_add);

        // securityOpt: concatenate, deduplicate.
        extend_dedup(&mut config.security_opt, &meta.security_opt);

        // privileged: OR.
        if meta.privileged == Some(true) {
            config.privileged = true;
        }

        // init: OR.
        if meta.init == Some(true) {
            config.init = true;
        }

        // containerEnv: later overrides earlier for same key.
        for (k, v) in &meta.container_env {
            config.container_env.insert(k.clone(), v.clone());
        }

        // entrypoint: chain in order.
        if let Some(ep) = &meta.entrypoint {
            config.entrypoints.push(ep.clone());
        }

        // Lifecycle commands: collect in install order.
        if let Some(cmd) = &meta.on_create_command {
            config.lifecycle.on_create.push(cmd.clone());
        }
        if let Some(cmd) = &meta.post_create_command {
            config.lifecycle.post_create.push(cmd.clone());
        }
        if let Some(cmd) = &meta.post_start_command {
            config.lifecycle.post_start.push(cmd.clone());
        }
        if let Some(cmd) = &meta.post_attach_command {
            config.lifecycle.post_attach.push(cmd.clone());
        }

        // Customizations: deep merge in install order.
        if let Some(cust) = &meta.customizations {
            config.customizations = deep_merge(&config.customizations, cust);
        }
    }

    config
}

/// Merge accumulated feature config with the user's `devcontainer.json`.
///
/// User settings take precedence for conflicting keys, but feature settings
/// are prepended/included where appropriate:
///
/// - `mounts`: feature mounts prepended before user mounts
/// - `capAdd`, `securityOpt`: feature + user values, deduplicated
/// - `privileged`, `init`: OR of feature and user values
/// - `containerEnv`: feature env first, user env overrides same keys
/// - lifecycle commands: feature commands before user commands
/// - `customizations`: deep merge, user overrides features
pub fn merge_with_devcontainer(
    feature_config: &FeatureContainerConfig,
    devcontainer: &serde_json::Value,
) -> FeatureContainerConfig {
    let obj = devcontainer.as_object();

    // Mounts: feature mounts prepended before user mounts.
    let mut mounts = feature_config.mounts.clone();
    if let Some(user_mounts) = obj.and_then(|o| o.get("mounts")).and_then(|v| v.as_array()) {
        for m in user_mounts {
            if let Some(s) = m.as_str() {
                mounts.push(s.to_string());
            }
        }
    }

    // capAdd: feature + user, deduplicated.
    let mut cap_add = feature_config.cap_add.clone();
    if let Some(user_caps) = obj.and_then(|o| o.get("capAdd")).and_then(|v| v.as_array()) {
        let user_strs: Vec<String> = user_caps
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        extend_dedup(&mut cap_add, &user_strs);
    }

    // securityOpt: feature + user, deduplicated.
    let mut security_opt = feature_config.security_opt.clone();
    if let Some(user_sec) = obj
        .and_then(|o| o.get("securityOpt"))
        .and_then(|v| v.as_array())
    {
        let user_strs: Vec<String> = user_sec
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        extend_dedup(&mut security_opt, &user_strs);
    }

    // privileged: OR.
    let user_privileged = obj
        .and_then(|o| o.get("privileged"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let privileged = feature_config.privileged || user_privileged;

    // init: OR.
    let user_init = obj
        .and_then(|o| o.get("init"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let init = feature_config.init || user_init;

    // containerEnv: feature env first, user overrides same keys.
    let mut container_env = feature_config.container_env.clone();
    if let Some(user_env) = obj
        .and_then(|o| o.get("containerEnv"))
        .and_then(|v| v.as_object())
    {
        for (k, v) in user_env {
            if let Some(s) = v.as_str() {
                container_env.insert(k.clone(), s.to_string());
            }
        }
    }

    // Entrypoints: carried through from feature config (devcontainer.json
    // doesn't have an entrypoints array; its entrypoint is handled separately).
    let entrypoints = feature_config.entrypoints.clone();

    // Lifecycle: feature commands before user commands.
    let lifecycle = merge_lifecycle(&feature_config.lifecycle, devcontainer);

    // Customizations: deep merge, user overrides features.
    let mut customizations = feature_config.customizations.clone();
    if let Some(user_cust) = obj.and_then(|o| o.get("customizations")) {
        customizations = deep_merge(&customizations, user_cust);
    }

    FeatureContainerConfig {
        mounts,
        cap_add,
        security_opt,
        privileged,
        init,
        container_env,
        entrypoints,
        lifecycle,
        customizations,
    }
}

/// Validate user-provided options against declared feature options.
///
/// All validation is advisory -- options are always passed through regardless.
/// Returns warnings for:
/// - Unknown option keys not declared in the feature metadata
/// - Type mismatches (e.g., string value for a boolean option)
/// - Enum values not in the declared allowed set
#[allow(clippy::implicit_hasher)]
pub fn validate_options(
    feature_id: &str,
    user_options: &HashMap<String, serde_json::Value>,
    declared_options: &HashMap<String, FeatureOption>,
) -> Vec<FeatureWarning> {
    let mut warnings = Vec::new();

    for (key, value) in user_options {
        let Some(decl) = declared_options.get(key) else {
            warnings.push(FeatureWarning::UnknownOption {
                feature_id: feature_id.to_string(),
                option: key.clone(),
            });
            continue;
        };

        // Type checking.
        match decl.option_type {
            OptionType::Boolean => {
                if !value.is_boolean() {
                    // Strings "true"/"false" are commonly accepted, but flag anything else.
                    let is_bool_string = value.as_str().is_some_and(|s| {
                        s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false")
                    });
                    if !is_bool_string {
                        warnings.push(FeatureWarning::TypeMismatch {
                            feature_id: feature_id.to_string(),
                            option: key.clone(),
                            expected: "boolean".to_string(),
                            got: value_type_name(value),
                        });
                    }
                }
            }
            OptionType::String => {
                // Strings accept anything that can be stringified, but check enum constraints.
                if let Some(enum_values) = &decl.enum_values {
                    let str_val = value_as_string(value);
                    if !enum_values.contains(&str_val) {
                        warnings.push(FeatureWarning::InvalidEnumValue {
                            feature_id: feature_id.to_string(),
                            option: key.clone(),
                            value: str_val,
                            allowed: enum_values.clone(),
                        });
                    }
                }
            }
        }
    }

    warnings
}

/// Extend a `Vec<String>` with new items, skipping duplicates.
fn extend_dedup(target: &mut Vec<String>, items: &[String]) {
    let existing: HashSet<String> = target.iter().cloned().collect();
    for item in items {
        if !existing.contains(item) {
            target.push(item.clone());
        }
    }
}

/// Deep merge two JSON values. `overlay` values override `base` values for
/// the same key; objects are merged recursively; non-object values are replaced.
fn deep_merge(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (k, v) in overlay_map {
                let entry = merged.entry(k.clone()).or_insert(serde_json::Value::Null);
                *entry = deep_merge(entry, v);
            }
            serde_json::Value::Object(merged)
        }
        // Non-object: overlay wins.
        (_, overlay) => overlay.clone(),
    }
}

/// Merge feature lifecycle commands with user devcontainer lifecycle commands.
/// Feature commands come before user commands.
fn merge_lifecycle(
    feature_lifecycle: &FeatureLifecycle,
    devcontainer: &serde_json::Value,
) -> FeatureLifecycle {
    let obj = devcontainer.as_object();

    FeatureLifecycle {
        on_create: merge_lifecycle_field(
            &feature_lifecycle.on_create,
            obj.and_then(|o| o.get("onCreateCommand")),
        ),
        post_create: merge_lifecycle_field(
            &feature_lifecycle.post_create,
            obj.and_then(|o| o.get("postCreateCommand")),
        ),
        post_start: merge_lifecycle_field(
            &feature_lifecycle.post_start,
            obj.and_then(|o| o.get("postStartCommand")),
        ),
        post_attach: merge_lifecycle_field(
            &feature_lifecycle.post_attach,
            obj.and_then(|o| o.get("postAttachCommand")),
        ),
    }
}

/// Merge a single lifecycle field: feature commands first, then user command (if present).
fn merge_lifecycle_field(
    feature_cmds: &[serde_json::Value],
    user_cmd: Option<&serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = feature_cmds.to_vec();
    if let Some(cmd) = user_cmd
        && !cmd.is_null()
    {
        result.push(cmd.clone());
    }
    result
}

/// Get a human-readable type name for a JSON value.
fn value_type_name(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "boolean".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::String(_) => "string".to_string(),
        serde_json::Value::Array(_) => "array".to_string(),
        serde_json::Value::Object(_) => "object".to_string(),
    }
}

/// Coerce a JSON value to its string representation for enum comparison.
fn value_as_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::types::{FeatureMetadata, ResolvedFeature};

    /// Build a minimal `ResolvedFeature` with the given metadata overrides.
    fn resolved(id: &str, meta: FeatureMetadata) -> ResolvedFeature {
        ResolvedFeature {
            id: id.to_string(),
            original_ref: id.to_string(),
            metadata: meta,
            user_options: HashMap::new(),
            artifact_dir: PathBuf::from("/tmp/features"),
            has_install_script: true,
        }
    }

    // =================================================================
    // Feature-to-feature merge tests
    // =================================================================

    #[test]
    fn mounts_concatenate_in_install_order() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    mounts: vec!["type=volume,src=a-vol,dst=/a".to_string()],
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    mounts: vec!["type=volume,src=b-vol,dst=/b".to_string()],
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        assert_eq!(
            config.mounts,
            vec![
                "type=volume,src=a-vol,dst=/a",
                "type=volume,src=b-vol,dst=/b",
            ]
        );
    }

    #[test]
    fn cap_add_concatenates_and_deduplicates() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    cap_add: vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()],
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    cap_add: vec!["SYS_PTRACE".to_string(), "SYS_CHROOT".to_string()],
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        assert_eq!(
            config.cap_add,
            vec!["SYS_PTRACE", "NET_ADMIN", "SYS_CHROOT"]
        );
    }

    #[test]
    fn security_opt_concatenates_and_deduplicates() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    security_opt: vec!["seccomp=unconfined".to_string()],
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    security_opt: vec![
                        "seccomp=unconfined".to_string(),
                        "apparmor=unconfined".to_string(),
                    ],
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        assert_eq!(
            config.security_opt,
            vec!["seccomp=unconfined", "apparmor=unconfined"]
        );
    }

    #[test]
    fn privileged_any_true_wins() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    privileged: Some(false),
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    privileged: Some(true),
                    ..Default::default()
                },
            ),
            resolved(
                "c",
                FeatureMetadata {
                    privileged: None,
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);
        assert!(config.privileged);
    }

    #[test]
    fn privileged_all_false_stays_false() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    privileged: Some(false),
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    privileged: None,
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);
        assert!(!config.privileged);
    }

    #[test]
    fn init_any_true_wins() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    init: None,
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    init: Some(true),
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);
        assert!(config.init);
    }

    #[test]
    fn container_env_later_overrides_earlier() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    container_env: HashMap::from([
                        ("FOO".to_string(), "from_a".to_string()),
                        ("BAR".to_string(), "from_a".to_string()),
                    ]),
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    container_env: HashMap::from([
                        ("FOO".to_string(), "from_b".to_string()),
                        ("BAZ".to_string(), "from_b".to_string()),
                    ]),
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        assert_eq!(config.container_env.get("FOO").unwrap(), "from_b");
        assert_eq!(config.container_env.get("BAR").unwrap(), "from_a");
        assert_eq!(config.container_env.get("BAZ").unwrap(), "from_b");
    }

    #[test]
    fn entrypoints_chain_in_install_order() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    entrypoint: Some("/usr/local/bin/a-init.sh".to_string()),
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    entrypoint: None,
                    ..Default::default()
                },
            ),
            resolved(
                "c",
                FeatureMetadata {
                    entrypoint: Some("/usr/local/bin/c-init.sh".to_string()),
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        assert_eq!(
            config.entrypoints,
            vec!["/usr/local/bin/a-init.sh", "/usr/local/bin/c-init.sh",]
        );
    }

    #[test]
    fn lifecycle_commands_collect_in_install_order() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    on_create_command: Some(json!("echo a-create")),
                    post_create_command: Some(json!("echo a-post-create")),
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    on_create_command: Some(json!("echo b-create")),
                    post_start_command: Some(json!(["echo", "b-start"])),
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        assert_eq!(
            config.lifecycle.on_create,
            vec![json!("echo a-create"), json!("echo b-create"),]
        );
        assert_eq!(
            config.lifecycle.post_create,
            vec![json!("echo a-post-create")]
        );
        assert_eq!(
            config.lifecycle.post_start,
            vec![json!(["echo", "b-start"])]
        );
        assert!(config.lifecycle.post_attach.is_empty());
    }

    #[test]
    fn customizations_deep_merged() {
        let features = vec![
            resolved(
                "a",
                FeatureMetadata {
                    customizations: Some(json!({
                        "vscode": {
                            "extensions": ["ext-a"],
                            "settings": { "a.setting": true }
                        }
                    })),
                    ..Default::default()
                },
            ),
            resolved(
                "b",
                FeatureMetadata {
                    customizations: Some(json!({
                        "vscode": {
                            "extensions": ["ext-b"],
                            "settings": { "b.setting": 42 }
                        }
                    })),
                    ..Default::default()
                },
            ),
        ];

        let config = merge_features(&features);

        // Deep merge: b's extensions replace a's (array is non-object, overlay wins),
        // but settings are merged at key level.
        let vscode = &config.customizations["vscode"];
        assert_eq!(vscode["extensions"], json!(["ext-b"]));
        assert_eq!(vscode["settings"]["a.setting"], json!(true));
        assert_eq!(vscode["settings"]["b.setting"], json!(42));
    }

    #[test]
    fn empty_features_produces_default_config() {
        let config = merge_features(&[]);

        assert!(config.mounts.is_empty());
        assert!(config.cap_add.is_empty());
        assert!(config.security_opt.is_empty());
        assert!(!config.privileged);
        assert!(!config.init);
        assert!(config.container_env.is_empty());
        assert!(config.entrypoints.is_empty());
        assert!(config.lifecycle.on_create.is_empty());
    }

    // =================================================================
    // Feature-to-devcontainer merge tests
    // =================================================================

    #[test]
    fn feature_mounts_prepend_before_user_mounts() {
        let feature_config = FeatureContainerConfig {
            mounts: vec!["type=volume,src=feat,dst=/feat".to_string()],
            ..Default::default()
        };
        let devcontainer = json!({
            "mounts": ["type=bind,src=/host,dst=/container"]
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(
            merged.mounts,
            vec![
                "type=volume,src=feat,dst=/feat",
                "type=bind,src=/host,dst=/container",
            ]
        );
    }

    #[test]
    fn user_container_env_overrides_feature_env() {
        let feature_config = FeatureContainerConfig {
            container_env: HashMap::from([
                ("SHARED".to_string(), "feature_value".to_string()),
                ("FEATURE_ONLY".to_string(), "yes".to_string()),
            ]),
            ..Default::default()
        };
        let devcontainer = json!({
            "containerEnv": {
                "SHARED": "user_value",
                "USER_ONLY": "hello"
            }
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(merged.container_env.get("SHARED").unwrap(), "user_value");
        assert_eq!(merged.container_env.get("FEATURE_ONLY").unwrap(), "yes");
        assert_eq!(merged.container_env.get("USER_ONLY").unwrap(), "hello");
    }

    #[test]
    fn feature_lifecycle_commands_before_user_commands() {
        let feature_config = FeatureContainerConfig {
            lifecycle: FeatureLifecycle {
                on_create: vec![json!("echo feature-create")],
                post_create: vec![json!("echo feature-post")],
                ..Default::default()
            },
            ..Default::default()
        };
        let devcontainer = json!({
            "onCreateCommand": "echo user-create",
            "postCreateCommand": "echo user-post"
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(
            merged.lifecycle.on_create,
            vec![json!("echo feature-create"), json!("echo user-create"),]
        );
        assert_eq!(
            merged.lifecycle.post_create,
            vec![json!("echo feature-post"), json!("echo user-post"),]
        );
    }

    #[test]
    fn feature_privileged_or_with_user_privileged() {
        // Feature false, user true => true.
        let feature_config = FeatureContainerConfig {
            privileged: false,
            ..Default::default()
        };
        let devcontainer = json!({ "privileged": true });
        let merged = merge_with_devcontainer(&feature_config, &devcontainer);
        assert!(merged.privileged);

        // Feature true, user false => true.
        let feature_config = FeatureContainerConfig {
            privileged: true,
            ..Default::default()
        };
        let devcontainer = json!({ "privileged": false });
        let merged = merge_with_devcontainer(&feature_config, &devcontainer);
        assert!(merged.privileged);

        // Both false => false.
        let feature_config = FeatureContainerConfig {
            privileged: false,
            ..Default::default()
        };
        let devcontainer = json!({ "privileged": false });
        let merged = merge_with_devcontainer(&feature_config, &devcontainer);
        assert!(!merged.privileged);
    }

    #[test]
    fn feature_init_or_with_user_init() {
        let feature_config = FeatureContainerConfig {
            init: false,
            ..Default::default()
        };
        let devcontainer = json!({ "init": true });
        let merged = merge_with_devcontainer(&feature_config, &devcontainer);
        assert!(merged.init);
    }

    #[test]
    fn cap_add_feature_plus_user_deduplicated() {
        let feature_config = FeatureContainerConfig {
            cap_add: vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()],
            ..Default::default()
        };
        let devcontainer = json!({
            "capAdd": ["SYS_PTRACE", "SYS_CHROOT"]
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(
            merged.cap_add,
            vec!["SYS_PTRACE", "NET_ADMIN", "SYS_CHROOT"]
        );
    }

    #[test]
    fn security_opt_feature_plus_user_deduplicated() {
        let feature_config = FeatureContainerConfig {
            security_opt: vec!["seccomp=unconfined".to_string()],
            ..Default::default()
        };
        let devcontainer = json!({
            "securityOpt": ["seccomp=unconfined", "label=disable"]
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(
            merged.security_opt,
            vec!["seccomp=unconfined", "label=disable"]
        );
    }

    #[test]
    fn customizations_user_overrides_features() {
        let feature_config = FeatureContainerConfig {
            customizations: json!({
                "vscode": {
                    "settings": { "editor.fontSize": 14, "feature.setting": true }
                }
            }),
            ..Default::default()
        };
        let devcontainer = json!({
            "customizations": {
                "vscode": {
                    "settings": { "editor.fontSize": 16, "user.setting": "yes" }
                }
            }
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        let settings = &merged.customizations["vscode"]["settings"];
        assert_eq!(settings["editor.fontSize"], json!(16));
        assert_eq!(settings["feature.setting"], json!(true));
        assert_eq!(settings["user.setting"], json!("yes"));
    }

    #[test]
    fn empty_devcontainer_preserves_feature_config() {
        let feature_config = FeatureContainerConfig {
            mounts: vec!["type=volume,src=v,dst=/v".to_string()],
            privileged: true,
            container_env: HashMap::from([("KEY".to_string(), "val".to_string())]),
            ..Default::default()
        };

        let merged = merge_with_devcontainer(&feature_config, &json!({}));

        assert_eq!(merged.mounts, feature_config.mounts);
        assert!(merged.privileged);
        assert_eq!(merged.container_env.get("KEY").unwrap(), "val");
    }

    // =================================================================
    // Option validation tests
    // =================================================================

    #[test]
    fn unknown_option_warns() {
        let declared = HashMap::new();
        let user = HashMap::from([("mystery".to_string(), json!("value"))]);

        let warnings = validate_options("test-feature", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::UnknownOption { feature_id, option } => {
                assert_eq!(feature_id, "test-feature");
                assert_eq!(option, "mystery");
            }
            other => panic!("expected UnknownOption, got {other:?}"),
        }
    }

    #[test]
    fn type_mismatch_boolean_gets_number() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);
        let user = HashMap::from([("flag".to_string(), json!(42))]);

        let warnings = validate_options("feat", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::TypeMismatch { expected, got, .. } => {
                assert_eq!(expected, "boolean");
                assert_eq!(got, "number");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn boolean_string_true_false_accepted() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);

        // "true" as string is accepted.
        let user = HashMap::from([("flag".to_string(), json!("true"))]);
        let warnings = validate_options("feat", &user, &declared);
        assert!(warnings.is_empty());

        // "false" as string is accepted.
        let user = HashMap::from([("flag".to_string(), json!("false"))]);
        let warnings = validate_options("feat", &user, &declared);
        assert!(warnings.is_empty());

        // "TRUE" case-insensitive.
        let user = HashMap::from([("flag".to_string(), json!("TRUE"))]);
        let warnings = validate_options("feat", &user, &declared);
        assert!(warnings.is_empty());
    }

    #[test]
    fn boolean_option_with_non_bool_string_warns() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);
        let user = HashMap::from([("flag".to_string(), json!("yes"))]);

        let warnings = validate_options("feat", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::TypeMismatch { expected, got, .. } => {
                assert_eq!(expected, "boolean");
                assert_eq!(got, "string");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn enum_value_not_in_allowed_set() {
        let declared = HashMap::from([(
            "version".to_string(),
            FeatureOption {
                option_type: OptionType::String,
                default: json!("lts"),
                description: None,
                enum_values: Some(vec![
                    "lts".to_string(),
                    "latest".to_string(),
                    "18".to_string(),
                ]),
            },
        )]);
        let user = HashMap::from([("version".to_string(), json!("99"))]);

        let warnings = validate_options("node", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::InvalidEnumValue { value, allowed, .. } => {
                assert_eq!(value, "99");
                assert_eq!(allowed, &vec!["lts", "latest", "18"]);
            }
            other => panic!("expected InvalidEnumValue, got {other:?}"),
        }
    }

    #[test]
    fn enum_value_in_allowed_set_no_warning() {
        let declared = HashMap::from([(
            "version".to_string(),
            FeatureOption {
                option_type: OptionType::String,
                default: json!("lts"),
                description: None,
                enum_values: Some(vec!["lts".to_string(), "latest".to_string()]),
            },
        )]);
        let user = HashMap::from([("version".to_string(), json!("lts"))]);

        let warnings = validate_options("node", &user, &declared);
        assert!(warnings.is_empty());
    }

    #[test]
    fn valid_options_no_warnings() {
        let declared = HashMap::from([
            (
                "version".to_string(),
                FeatureOption {
                    option_type: OptionType::String,
                    default: json!("lts"),
                    description: None,
                    enum_values: None,
                },
            ),
            (
                "install_tools".to_string(),
                FeatureOption {
                    option_type: OptionType::Boolean,
                    default: json!(true),
                    description: None,
                    enum_values: None,
                },
            ),
        ]);
        let user = HashMap::from([
            ("version".to_string(), json!("18")),
            ("install_tools".to_string(), json!(false)),
        ]);

        let warnings = validate_options("node", &user, &declared);
        assert!(warnings.is_empty());
    }

    #[test]
    fn multiple_warnings_collected() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);
        let user = HashMap::from([
            ("flag".to_string(), json!(42)),
            ("unknown1".to_string(), json!("x")),
            ("unknown2".to_string(), json!("y")),
        ]);

        let warnings = validate_options("feat", &user, &declared);

        // Should have 3 warnings: one TypeMismatch + two UnknownOption.
        assert_eq!(warnings.len(), 3);
    }

    // =================================================================
    // deep_merge unit tests
    // =================================================================

    #[test]
    fn deep_merge_nested_objects() {
        let base = json!({ "a": { "b": 1, "c": 2 } });
        let overlay = json!({ "a": { "c": 3, "d": 4 } });

        let result = deep_merge(&base, &overlay);

        assert_eq!(result, json!({ "a": { "b": 1, "c": 3, "d": 4 } }));
    }

    #[test]
    fn deep_merge_overlay_replaces_non_object() {
        let base = json!({ "key": "old" });
        let overlay = json!({ "key": "new" });

        let result = deep_merge(&base, &overlay);

        assert_eq!(result, json!({ "key": "new" }));
    }

    #[test]
    fn deep_merge_base_null() {
        let base = serde_json::Value::Null;
        let overlay = json!({ "key": "value" });

        let result = deep_merge(&base, &overlay);

        assert_eq!(result, json!({ "key": "value" }));
    }
}
