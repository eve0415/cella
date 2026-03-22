use crate::types::{FeatureContainerConfig, LifecycleEntry, ResolvedFeature};

use super::helpers::{deep_merge, extend_dedup};

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
pub fn merge_features(
    features: &[ResolvedFeature],
    base: Option<FeatureContainerConfig>,
) -> FeatureContainerConfig {
    let mut config = base.unwrap_or_default();

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

        // Lifecycle commands: collect in install order with origin tracking.
        if let Some(cmd) = &meta.on_create_command {
            config.lifecycle.on_create.push(LifecycleEntry {
                origin: feature.id.clone(),
                command: cmd.clone(),
            });
        }
        if let Some(cmd) = &meta.post_create_command {
            config.lifecycle.post_create.push(LifecycleEntry {
                origin: feature.id.clone(),
                command: cmd.clone(),
            });
        }
        if let Some(cmd) = &meta.post_start_command {
            config.lifecycle.post_start.push(LifecycleEntry {
                origin: feature.id.clone(),
                command: cmd.clone(),
            });
        }
        if let Some(cmd) = &meta.post_attach_command {
            config.lifecycle.post_attach.push(LifecycleEntry {
                origin: feature.id.clone(),
                command: cmd.clone(),
            });
        }

        // Customizations: deep merge in install order.
        if let Some(cust) = &meta.customizations {
            config.customizations = deep_merge(&config.customizations, cust);
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
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

        let config = merge_features(&features, None);

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

        let config = merge_features(&features, None);

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

        let config = merge_features(&features, None);

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

        let config = merge_features(&features, None);
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

        let config = merge_features(&features, None);
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

        let config = merge_features(&features, None);
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

        let config = merge_features(&features, None);

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

        let config = merge_features(&features, None);

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

        let config = merge_features(&features, None);

        assert_eq!(config.lifecycle.on_create.len(), 2);
        assert_eq!(config.lifecycle.on_create[0].origin, "a");
        assert_eq!(
            config.lifecycle.on_create[0].command,
            json!("echo a-create")
        );
        assert_eq!(config.lifecycle.on_create[1].origin, "b");
        assert_eq!(
            config.lifecycle.on_create[1].command,
            json!("echo b-create")
        );

        assert_eq!(config.lifecycle.post_create.len(), 1);
        assert_eq!(
            config.lifecycle.post_create[0].command,
            json!("echo a-post-create")
        );

        assert_eq!(config.lifecycle.post_start.len(), 1);
        assert_eq!(
            config.lifecycle.post_start[0].command,
            json!(["echo", "b-start"])
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

        let config = merge_features(&features, None);

        // Deep merge: b's extensions replace a's (array is non-object, overlay wins),
        // but settings are merged at key level.
        let vscode = &config.customizations["vscode"];
        assert_eq!(vscode["extensions"], json!(["ext-b"]));
        assert_eq!(vscode["settings"]["a.setting"], json!(true));
        assert_eq!(vscode["settings"]["b.setting"], json!(42));
    }

    #[test]
    fn empty_features_produces_default_config() {
        let config = merge_features(&[], None);

        assert!(config.mounts.is_empty());
        assert!(config.cap_add.is_empty());
        assert!(config.security_opt.is_empty());
        assert!(!config.privileged);
        assert!(!config.init);
        assert!(config.container_env.is_empty());
        assert!(config.entrypoints.is_empty());
        assert!(config.lifecycle.on_create.is_empty());
    }
}
