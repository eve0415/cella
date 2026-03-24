use crate::types::{FeatureContainerConfig, FeatureLifecycle, LifecycleEntry};

use super::helpers::{deep_merge, extend_dedup};

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
            } else if let Some(mount_obj) = m.as_object() {
                let mt = mount_obj
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("bind");
                let src = mount_obj
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let tgt = mount_obj
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !tgt.is_empty() {
                    mounts.push(format!("type={mt},source={src},target={tgt}"));
                }
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
        update_content: merge_lifecycle_field(
            &feature_lifecycle.update_content,
            obj.and_then(|o| o.get("updateContentCommand")),
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

/// Merge a single lifecycle field: feature entries first, then user command (if present).
fn merge_lifecycle_field(
    feature_entries: &[LifecycleEntry],
    user_cmd: Option<&serde_json::Value>,
) -> Vec<LifecycleEntry> {
    let mut result: Vec<LifecycleEntry> = feature_entries.to_vec();
    if let Some(cmd) = user_cmd
        && !cmd.is_null()
    {
        result.push(LifecycleEntry {
            origin: "devcontainer.json".to_string(),
            command: cmd.clone(),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

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
                on_create: vec![LifecycleEntry {
                    origin: "feat-a".to_string(),
                    command: json!("echo feature-create"),
                }],
                post_create: vec![LifecycleEntry {
                    origin: "feat-a".to_string(),
                    command: json!("echo feature-post"),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let devcontainer = json!({
            "onCreateCommand": "echo user-create",
            "postCreateCommand": "echo user-post"
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(merged.lifecycle.on_create.len(), 2);
        assert_eq!(merged.lifecycle.on_create[0].origin, "feat-a");
        assert_eq!(
            merged.lifecycle.on_create[0].command,
            json!("echo feature-create")
        );
        assert_eq!(merged.lifecycle.on_create[1].origin, "devcontainer.json");
        assert_eq!(
            merged.lifecycle.on_create[1].command,
            json!("echo user-create")
        );

        assert_eq!(merged.lifecycle.post_create.len(), 2);
        assert_eq!(
            merged.lifecycle.post_create[0].command,
            json!("echo feature-post")
        );
        assert_eq!(
            merged.lifecycle.post_create[1].command,
            json!("echo user-post")
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

    #[test]
    fn object_format_mounts_merged_from_devcontainer() {
        let feature_config = FeatureContainerConfig {
            mounts: vec!["type=volume,src=feat-vol,dst=/feat".to_string()],
            ..Default::default()
        };
        let devcontainer = json!({
            "mounts": [
                "type=bind,src=/host-str,dst=/str-mount",
                {"type": "bind", "source": "/host-obj", "target": "/obj-mount"},
            ]
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);

        assert_eq!(merged.mounts.len(), 3);
        assert_eq!(merged.mounts[0], "type=volume,src=feat-vol,dst=/feat");
        assert_eq!(merged.mounts[1], "type=bind,src=/host-str,dst=/str-mount");
        assert_eq!(
            merged.mounts[2],
            "type=bind,source=/host-obj,target=/obj-mount"
        );
    }

    #[test]
    fn object_format_mount_without_target_skipped() {
        let feature_config = FeatureContainerConfig::default();
        let devcontainer = json!({
            "mounts": [
                {"type": "bind", "source": "/src"},
            ]
        });

        let merged = merge_with_devcontainer(&feature_config, &devcontainer);
        assert!(merged.mounts.is_empty());
    }
}
