//! Dockerfile generation for devcontainer feature layers.
//!
//! Produces a Dockerfile that installs resolved features into a base image,
//! matching the devcontainer CLI spec's Dockerfile template exactly.

use std::fmt::Write;

use crate::types::ResolvedFeature;

/// Generate a Dockerfile that installs the given features into a base image.
///
/// The generated Dockerfile follows the devcontainer CLI spec:
/// 1. `ARG` declarations before `FROM`
/// 2. `FROM <base> AS dev_containers_target_stage`
/// 3. Per-feature `COPY` + `RUN` blocks (only for features with `install.sh`)
/// 4. Cleanup of `/tmp/dev-container-features`
/// 5. `USER` reset to the original image user
///
/// Features without `install.sh` (metadata-only) are skipped entirely.
pub fn generate_dockerfile(
    base_image: &str,
    image_user: &str,
    features: &[ResolvedFeature],
) -> String {
    let mut out = String::new();

    // ARG declarations before FROM
    writeln!(out, "ARG _DEV_CONTAINERS_BASE_IMAGE={base_image}").unwrap();
    writeln!(out, "ARG _DEV_CONTAINERS_IMAGE_USER={image_user}").unwrap();
    writeln!(out, "ARG _DEV_CONTAINERS_FEATURE_CONTENT_SOURCE").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage"
    )
    .unwrap();

    // Track current USER context to avoid redundant directives.
    let mut current_user = image_user;

    let installable: Vec<&ResolvedFeature> =
        features.iter().filter(|f| f.has_install_script).collect();

    // Emit feature install blocks
    for feature in &installable {
        let target_user = feature.metadata.container_user.as_deref().unwrap_or("root");

        if current_user != target_user {
            writeln!(out).unwrap();
            writeln!(out, "USER {target_user}").unwrap();
            current_user = target_user;
        }

        // Emit ENV instructions for feature containerEnv (before COPY+RUN so
        // install scripts and subsequent layers see the values, and ${VAR}
        // expansion works correctly during the Docker build).
        if !feature.metadata.container_env.is_empty() {
            let mut keys: Vec<&String> = feature.metadata.container_env.keys().collect();
            keys.sort();
            writeln!(out).unwrap();
            for key in keys {
                let value = &feature.metadata.container_env[key];
                let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                writeln!(out, "ENV {key}=\"{escaped}\"").unwrap();
            }
        }

        writeln!(out).unwrap();
        writeln!(
            out,
            "# Feature: {} ({})",
            feature.metadata.id, feature.original_ref
        )
        .unwrap();
        writeln!(
            out,
            "COPY --chown=root:root {id}/ /tmp/dev-container-features/{id}/",
            id = feature.id
        )
        .unwrap();

        // Build the RUN block with option exports
        let env_lines = build_env_exports(feature);
        write!(
            out,
            "RUN chmod +x /tmp/dev-container-features/{id}/install.sh",
            id = feature.id
        )
        .unwrap();
        write!(
            out,
            " \\\n    && cd /tmp/dev-container-features/{id}",
            id = feature.id
        )
        .unwrap();
        for env_line in &env_lines {
            write!(out, " \\\n    && export {env_line}").unwrap();
        }
        writeln!(out, " \\\n    && ./install.sh").unwrap();
    }

    // Entrypoint init script
    let has_entrypoints = features.iter().any(|f| f.metadata.entrypoint.is_some());

    if has_entrypoints {
        writeln!(out).unwrap();
        writeln!(out, "# Entrypoint init script").unwrap();
        writeln!(out, "COPY docker-init.sh /usr/local/share/docker-init.sh").unwrap();
        writeln!(out, "RUN chmod +x /usr/local/share/docker-init.sh").unwrap();
    }

    // Cleanup (only if we installed anything)
    if !installable.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "# Cleanup").unwrap();
        writeln!(out, "RUN rm -rf /tmp/dev-container-features").unwrap();
    }

    // Reset USER to original image user if changed
    if current_user != image_user {
        writeln!(out).unwrap();
        writeln!(out, "# Reset user").unwrap();
        writeln!(out, "USER {image_user}").unwrap();
    }

    out
}

/// Generate the entrypoint init script content for features with entrypoints.
///
/// Returns `None` if no features declare entrypoints.
pub fn generate_entrypoint_script(features: &[ResolvedFeature]) -> Option<String> {
    let entrypoints: Vec<&str> = features
        .iter()
        .filter_map(|f| f.metadata.entrypoint.as_deref())
        .collect();

    if entrypoints.is_empty() {
        return None;
    }

    let mut out = String::new();
    writeln!(out, "#!/bin/sh").unwrap();
    writeln!(out, "# Entrypoints from devcontainer features").unwrap();
    for ep in &entrypoints {
        writeln!(out, "{ep}").unwrap();
    }
    writeln!(out, "exec \"$@\"").unwrap();

    Some(out)
}

/// Build sorted environment variable export strings for a feature's options.
///
/// User-provided options take precedence. For options not provided by the user,
/// the default value from the feature's declared options is used. Option names
/// are converted to UPPERCASE for the env var, matching the original devcontainer
/// CLI behavior.
fn build_env_exports(feature: &ResolvedFeature) -> Vec<String> {
    let mut exports = Vec::new();

    // Collect all option names: union of declared options and user-provided options.
    let mut all_keys: Vec<String> = feature
        .metadata
        .options
        .keys()
        .chain(feature.user_options.keys())
        .cloned()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    // Sort for deterministic output.
    all_keys.sort();

    for key in &all_keys {
        let value = if let Some(user_val) = feature.user_options.get(key) {
            json_value_to_string(user_val)
        } else if let Some(opt) = feature.metadata.options.get(key) {
            json_value_to_string(&opt.default)
        } else {
            // User-provided key with no declared option and no value -- skip
            continue;
        };

        let env_name = key.to_uppercase();
        exports.push(format!("{env_name}=\"{value}\""));
    }

    exports
}

/// Convert a JSON value to its string representation for env var export.
fn json_value_to_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::types::{FeatureMetadata, FeatureOption, OptionType};

    /// Helper to build a `ResolvedFeature` with common defaults.
    fn make_feature(
        id: &str,
        original_ref: &str,
        has_install_script: bool,
        user_options: HashMap<String, serde_json::Value>,
        metadata: FeatureMetadata,
    ) -> ResolvedFeature {
        ResolvedFeature {
            id: id.to_string(),
            original_ref: original_ref.to_string(),
            metadata,
            user_options,
            artifact_dir: PathBuf::from(format!("/tmp/features/{id}")),
            has_install_script,
        }
    }

    fn make_option(default: serde_json::Value) -> FeatureOption {
        FeatureOption {
            option_type: OptionType::String,
            default,
            description: None,
            enum_values: None,
        }
    }

    // ---------------------------------------------------------------
    // Single feature with options
    // ---------------------------------------------------------------

    #[test]
    fn single_feature_with_options() {
        let mut options = HashMap::new();
        options.insert("version".to_string(), make_option(serde_json::json!("lts")));
        options.insert(
            "nodeGypDependencies".to_string(),
            make_option(serde_json::json!(true)),
        );

        let mut user_options = HashMap::new();
        user_options.insert("version".to_string(), serde_json::json!("18"));

        let features = vec![make_feature(
            "node",
            "ghcr.io/devcontainers/features/node:1",
            true,
            user_options,
            FeatureMetadata {
                id: "node".to_string(),
                options,
                ..Default::default()
            },
        )];

        let result = generate_dockerfile(
            "mcr.microsoft.com/devcontainers/base:ubuntu",
            "vscode",
            &features,
        );
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Multiple features in install order
    // ---------------------------------------------------------------

    #[test]
    fn multiple_features_in_order() {
        let mut node_options = HashMap::new();
        node_options.insert("version".to_string(), make_option(serde_json::json!("lts")));

        let mut python_options = HashMap::new();
        python_options.insert(
            "version".to_string(),
            make_option(serde_json::json!("3.11")),
        );

        let features = vec![
            make_feature(
                "node",
                "ghcr.io/devcontainers/features/node:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "node".to_string(),
                    options: node_options,
                    ..Default::default()
                },
            ),
            make_feature(
                "python",
                "ghcr.io/devcontainers/features/python:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "python".to_string(),
                    options: python_options,
                    ..Default::default()
                },
            ),
        ];

        let result = generate_dockerfile(
            "mcr.microsoft.com/devcontainers/base:ubuntu",
            "vscode",
            &features,
        );
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Feature without install.sh (metadata-only) is skipped
    // ---------------------------------------------------------------

    #[test]
    fn metadata_only_feature_skipped() {
        let features = vec![
            make_feature(
                "node",
                "ghcr.io/devcontainers/features/node:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "node".to_string(),
                    ..Default::default()
                },
            ),
            make_feature(
                "metadata-only",
                "ghcr.io/example/metadata-only:1",
                false,
                HashMap::new(),
                FeatureMetadata {
                    id: "metadata-only".to_string(),
                    ..Default::default()
                },
            ),
        ];

        let result = generate_dockerfile("ubuntu:22.04", "root", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Feature with containerUser sets different USER directive
    // ---------------------------------------------------------------

    #[test]
    fn feature_with_container_user() {
        let features = vec![make_feature(
            "custom-tool",
            "ghcr.io/example/custom-tool:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "custom-tool".to_string(),
                container_user: Some("developer".to_string()),
                ..Default::default()
            },
        )];

        let result = generate_dockerfile("ubuntu:22.04", "vscode", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // USER reset at end to original image user
    // ---------------------------------------------------------------

    #[test]
    fn user_reset_at_end() {
        let features = vec![make_feature(
            "tool",
            "ghcr.io/example/tool:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "tool".to_string(),
                ..Default::default()
            },
        )];

        // Image user is "vscode", feature runs as root (default).
        // Should see USER root at start and USER vscode at end.
        let result = generate_dockerfile("ubuntu:22.04", "vscode", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Feature with no options (empty env vars in RUN)
    // ---------------------------------------------------------------

    #[test]
    fn feature_with_no_options() {
        let features = vec![make_feature(
            "minimal",
            "ghcr.io/example/minimal:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "minimal".to_string(),
                ..Default::default()
            },
        )];

        let result = generate_dockerfile("ubuntu:22.04", "root", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // No USER root emitted when image user is already root
    // ---------------------------------------------------------------

    #[test]
    fn no_redundant_user_root() {
        let features = vec![make_feature(
            "tool",
            "ghcr.io/example/tool:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "tool".to_string(),
                ..Default::default()
            },
        )];

        // Image user is root, feature runs as root => no USER directive needed.
        let result = generate_dockerfile("ubuntu:22.04", "root", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // All features are metadata-only => no COPY/RUN/cleanup blocks
    // ---------------------------------------------------------------

    #[test]
    fn all_metadata_only() {
        let features = vec![
            make_feature(
                "a",
                "ghcr.io/example/a:1",
                false,
                HashMap::new(),
                FeatureMetadata {
                    id: "a".to_string(),
                    ..Default::default()
                },
            ),
            make_feature(
                "b",
                "ghcr.io/example/b:1",
                false,
                HashMap::new(),
                FeatureMetadata {
                    id: "b".to_string(),
                    ..Default::default()
                },
            ),
        ];

        let result = generate_dockerfile("ubuntu:22.04", "vscode", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Mixed USER transitions across features
    // ---------------------------------------------------------------

    #[test]
    fn mixed_user_transitions() {
        let features = vec![
            make_feature(
                "root-tool",
                "ghcr.io/example/root-tool:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "root-tool".to_string(),
                    ..Default::default()
                },
            ),
            make_feature(
                "user-tool",
                "ghcr.io/example/user-tool:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "user-tool".to_string(),
                    container_user: Some("developer".to_string()),
                    ..Default::default()
                },
            ),
            make_feature(
                "another-root",
                "ghcr.io/example/another-root:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "another-root".to_string(),
                    ..Default::default()
                },
            ),
        ];

        let result = generate_dockerfile("ubuntu:22.04", "vscode", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Entrypoint init script generation
    // ---------------------------------------------------------------

    #[test]
    fn entrypoint_script_generated() {
        let features = vec![
            make_feature(
                "feature-a",
                "ghcr.io/example/feature-a:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "feature-a".to_string(),
                    entrypoint: Some("/usr/local/share/feature-a-init.sh".to_string()),
                    ..Default::default()
                },
            ),
            make_feature(
                "feature-b",
                "ghcr.io/example/feature-b:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "feature-b".to_string(),
                    entrypoint: Some("/usr/local/share/feature-b-init.sh".to_string()),
                    ..Default::default()
                },
            ),
        ];

        let script = generate_entrypoint_script(&features).unwrap();
        insta::assert_snapshot!(script);
    }

    // ---------------------------------------------------------------
    // No entrypoint script when no features have entrypoints
    // ---------------------------------------------------------------

    #[test]
    fn no_entrypoint_script_when_none_declared() {
        let features = vec![make_feature(
            "tool",
            "ghcr.io/example/tool:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "tool".to_string(),
                ..Default::default()
            },
        )];

        assert!(generate_entrypoint_script(&features).is_none());
    }

    // ---------------------------------------------------------------
    // Dockerfile includes entrypoint COPY when features have entrypoints
    // ---------------------------------------------------------------

    #[test]
    fn dockerfile_with_entrypoint() {
        let features = vec![make_feature(
            "feature-a",
            "ghcr.io/example/feature-a:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "feature-a".to_string(),
                entrypoint: Some("/usr/local/share/feature-a-init.sh".to_string()),
                ..Default::default()
            },
        )];

        let result = generate_dockerfile("ubuntu:22.04", "root", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Entrypoint from metadata-only feature still appears in script
    // ---------------------------------------------------------------

    #[test]
    fn entrypoint_from_metadata_only_feature() {
        let features = vec![
            make_feature(
                "installer",
                "ghcr.io/example/installer:1",
                true,
                HashMap::new(),
                FeatureMetadata {
                    id: "installer".to_string(),
                    ..Default::default()
                },
            ),
            make_feature(
                "meta",
                "ghcr.io/example/meta:1",
                false,
                HashMap::new(),
                FeatureMetadata {
                    id: "meta".to_string(),
                    entrypoint: Some("/usr/local/share/meta-init.sh".to_string()),
                    ..Default::default()
                },
            ),
        ];

        let script = generate_entrypoint_script(&features).unwrap();
        insta::assert_snapshot!(script);

        // Dockerfile should still have COPY for init script
        let dockerfile = generate_dockerfile("ubuntu:22.04", "root", &features);
        insta::assert_snapshot!("dockerfile_with_metadata_only_entrypoint", dockerfile);
    }

    // ---------------------------------------------------------------
    // Feature with containerEnv emits ENV instructions
    // ---------------------------------------------------------------

    #[test]
    fn feature_with_container_env() {
        let features = vec![make_feature(
            "node",
            "ghcr.io/devcontainers/features/node:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "node".to_string(),
                container_env: HashMap::from([
                    ("NVM_DIR".to_string(), "/usr/local/share/nvm".to_string()),
                    (
                        "PATH".to_string(),
                        "/usr/local/share/nvm/current/bin:${PATH}".to_string(),
                    ),
                ]),
                ..Default::default()
            },
        )];

        let result = generate_dockerfile("ubuntu:22.04", "root", &features);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Empty features list
    // ---------------------------------------------------------------

    #[test]
    fn empty_features_list() {
        let result = generate_dockerfile("ubuntu:22.04", "vscode", &[]);
        insta::assert_snapshot!(result);
    }
}
