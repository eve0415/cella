//! Dockerfile generation for devcontainer feature layers.
//!
//! Produces a Dockerfile that installs resolved features into a base image,
//! matching the devcontainer CLI spec's Dockerfile template exactly.

use std::fmt::Write;

use crate::reference::feature_id_without_version;
use crate::types::ResolvedFeature;

/// Named build context for feature content in Docker Compose builds.
///
/// When `use_named_content_source` is enabled, all `COPY` instructions reference
/// this named context via `--from=`, allowing Docker Compose `additional_contexts`
/// to provide the features staging directory while keeping the original service's
/// build context for the user's Dockerfile stage.
pub const FEATURE_CONTENT_SOURCE: &str = "dev_containers_feature_content_source";

/// Generate a Dockerfile that installs the given features into a base image.
///
/// The generated Dockerfile follows the devcontainer CLI spec:
/// 1. `ARG` declarations before `FROM`
/// 2. `FROM <base> AS dev_containers_target_stage`
/// 3. `USER root` for all feature installs
/// 4. Builtin env var resolution (`COPY` + `RUN`)
/// 5. Per-feature `COPY` + wrapper script `RUN` blocks (only for features with `install.sh`)
/// 6. Per-feature cleanup after each install
/// 7. Entrypoint init script (if any)
/// 8. Final cleanup of `/tmp/dev-container-features`
/// 9. `USER` reset via build arg
///
/// Features without `install.sh` (metadata-only) are skipped entirely.
pub fn generate_dockerfile(
    base_image: &str,
    image_user: &str,
    container_user: &str,
    remote_user: &str,
    features: &[ResolvedFeature],
    use_named_content_source: bool,
) -> String {
    let mut out = String::new();

    write_preamble(&mut out, base_image, image_user);

    let installable: Vec<&ResolvedFeature> =
        features.iter().filter(|f| f.has_install_script).collect();

    if !installable.is_empty() {
        write_builtin_env_resolution(
            &mut out,
            container_user,
            remote_user,
            use_named_content_source,
        );
        write_feature_install_blocks(&mut out, &installable, use_named_content_source);
    }

    write_entrypoint_section(&mut out, features, use_named_content_source);

    if !installable.is_empty() {
        write_cleanup_and_user_reset(&mut out);
    }

    out
}

/// Write ARG declarations and FROM line (the Dockerfile preamble).
fn write_preamble(out: &mut String, base_image: &str, image_user: &str) {
    writeln!(out, "ARG _DEV_CONTAINERS_BASE_IMAGE={base_image}").unwrap();
    writeln!(out, "ARG _DEV_CONTAINERS_IMAGE_USER={image_user}").unwrap();
    writeln!(out, "ARG _DEV_CONTAINERS_FEATURE_CONTENT_SOURCE").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage"
    )
    .unwrap();
}

/// Write USER root, COPY builtin env file, and RUN to resolve home directories.
fn write_builtin_env_resolution(
    out: &mut String,
    container_user: &str,
    remote_user: &str,
    use_named_content_source: bool,
) {
    let from_clause = if use_named_content_source {
        format!("--from={FEATURE_CONTENT_SOURCE} ")
    } else {
        String::new()
    };

    writeln!(out).unwrap();
    writeln!(out, "USER root").unwrap();

    writeln!(out).unwrap();
    writeln!(
        out,
        "COPY {from_clause}devcontainer-features.builtin.env \
         /tmp/dev-container-features/devcontainer-features.builtin.env"
    )
    .unwrap();
    write!(
        out,
        "RUN echo \"_CONTAINER_USER_HOME=$( \
         (command -v getent >/dev/null 2>&1 && getent passwd '{container_user}' \
         || grep -E '^{container_user}:' /etc/passwd || true) \
         | cut -d: -f6)\" \
         >> /tmp/dev-container-features/devcontainer-features.builtin.env"
    )
    .unwrap();
    writeln!(
        out,
        " \\\n    && echo \"_REMOTE_USER_HOME=$( \
         (command -v getent >/dev/null 2>&1 && getent passwd '{remote_user}' \
         || grep -E '^{remote_user}:' /etc/passwd || true) \
         | cut -d: -f6)\" \
         >> /tmp/dev-container-features/devcontainer-features.builtin.env"
    )
    .unwrap();
}

/// Write per-feature COPY + RUN install blocks (including containerEnv ENV lines).
fn write_feature_install_blocks(
    out: &mut String,
    installable: &[&ResolvedFeature],
    use_named_content_source: bool,
) {
    let from_clause = if use_named_content_source {
        format!("--from={FEATURE_CONTENT_SOURCE} ")
    } else {
        String::new()
    };

    for feature in installable {
        write_feature_container_env(out, feature);

        writeln!(out).unwrap();
        writeln!(
            out,
            "# Feature: {} ({})",
            feature.metadata.id, feature.original_ref
        )
        .unwrap();
        writeln!(
            out,
            "COPY {from_clause}--chown=root:root {id}/ /tmp/dev-container-features/{id}/",
            id = feature.id
        )
        .unwrap();
        write!(
            out,
            "RUN chmod -R 0755 /tmp/dev-container-features/{id}",
            id = feature.id
        )
        .unwrap();
        write!(
            out,
            " \\\n    && cd /tmp/dev-container-features/{id}",
            id = feature.id
        )
        .unwrap();
        write!(
            out,
            " \\\n    && chmod +x ./devcontainer-features-install.sh"
        )
        .unwrap();
        write!(out, " \\\n    && ./devcontainer-features-install.sh").unwrap();
        writeln!(
            out,
            " \\\n    && rm -rf /tmp/dev-container-features/{id}",
            id = feature.id
        )
        .unwrap();
    }
}

/// Write ENV instructions for a feature's `containerEnv` (sorted by key).
fn write_feature_container_env(out: &mut String, feature: &ResolvedFeature) {
    if feature.metadata.container_env.is_empty() {
        return;
    }

    let mut keys: Vec<&String> = feature.metadata.container_env.keys().collect();
    keys.sort();
    writeln!(out).unwrap();
    for key in keys {
        let value = &feature.metadata.container_env[key];
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(out, "ENV {key}=\"{escaped}\"").unwrap();
    }
}

/// Write entrypoint init script COPY+RUN if any feature declares an entrypoint.
fn write_entrypoint_section(
    out: &mut String,
    features: &[ResolvedFeature],
    use_named_content_source: bool,
) {
    let has_entrypoints = features.iter().any(|f| f.metadata.entrypoint.is_some());

    if has_entrypoints {
        let from_clause = if use_named_content_source {
            format!("--from={FEATURE_CONTENT_SOURCE} ")
        } else {
            String::new()
        };
        writeln!(out).unwrap();
        writeln!(out, "# Entrypoint init script").unwrap();
        writeln!(
            out,
            "COPY {from_clause}docker-init.sh /usr/local/share/docker-init.sh"
        )
        .unwrap();
        writeln!(out, "RUN chmod +x /usr/local/share/docker-init.sh").unwrap();
    }
}

/// Write final cleanup of /tmp/dev-container-features and USER reset via build arg.
fn write_cleanup_and_user_reset(out: &mut String) {
    writeln!(out).unwrap();
    writeln!(
        out,
        "RUN rm -rf /tmp/dev-container-features && chmod 1777 /tmp"
    )
    .unwrap();

    writeln!(out).unwrap();
    // Re-declare the ARG without a default so the features target stage
    // inherits the global-scope `ARG _DEV_CONTAINERS_IMAGE_USER` injected by
    // `cella_compose::generate_combined_dockerfile`. A stage-local default
    // here would shadow the global value and force the container to root.
    writeln!(out, "ARG _DEV_CONTAINERS_IMAGE_USER").unwrap();
    writeln!(out, "USER $_DEV_CONTAINERS_IMAGE_USER").unwrap();
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

/// Generate `devcontainer-features.builtin.env` content.
///
/// Contains `_CONTAINER_USER` and `_REMOTE_USER` variables that feature install
/// scripts use to configure user-specific settings (e.g., `usermod -aG`).
/// Home directory variables (`_CONTAINER_USER_HOME`, `_REMOTE_USER_HOME`) are
/// resolved at build time via a `RUN` command in the generated Dockerfile.
pub fn generate_builtin_env(container_user: &str, remote_user: &str) -> String {
    format!("_CONTAINER_USER={container_user}\n_REMOTE_USER={remote_user}\n")
}

/// Generate per-feature `devcontainer-features.env` content (option variables).
///
/// User-provided options take precedence over declared defaults. Option names
/// are converted to UPPERCASE, matching the original devcontainer CLI behavior.
/// The resulting file is sourced with `set -a` in the wrapper script.
pub fn generate_feature_env(feature: &ResolvedFeature) -> String {
    feature_env_lines(feature)
        .into_iter()
        .fold(String::new(), |mut acc, l| {
            acc.push_str(&l);
            acc.push('\n');
            acc
        })
}

/// Build the per-feature option env-var lines as a `Vec<String>`.
///
/// Each entry is a `KEY="value"` string without a trailing newline. Used both
/// by [`generate_feature_env`] (joined with newlines) and by
/// [`generate_wrapper_script`] (indented into the header block).
fn feature_env_lines(feature: &ResolvedFeature) -> Vec<String> {
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

    let mut lines = Vec::new();
    for key in &all_keys {
        let value = if let Some(user_val) = feature.user_options.get(key) {
            json_value_to_string(user_val)
        } else if let Some(opt) = feature.metadata.options.get(key) {
            // No `default` declared (absent → Null): omit the key entirely,
            // matching the official `'default' in options[key]` guard. An
            // explicit empty-string default still emits `KEY=""`.
            if opt.default.is_null() {
                continue;
            }
            json_value_to_string(&opt.default)
        } else {
            // User-provided key with no declared option and no value -- skip
            continue;
        };
        lines.push(format!("{}=\"{value}\"", option_key_to_env_var(key)));
    }

    lines
}

/// Replace every `'` in a string with `'\''` so the value can be safely
/// embedded inside single quotes in a POSIX shell script.
fn escape_quotes_for_shell(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Build the `warningHeader` string that appears inside the `echo` slot of the
/// wrapper script, mirroring the official `getFeatureInstallWrapperScript`.
///
/// The returned string is empty when no warnings apply; otherwise it contains
/// one or both of:
/// - Deprecation line (WITH trailing `\n` so the shell echoes a blank line
///   after it, matching the official output).
/// - Rename line (NO trailing newline), when the user's ref leaf differs from
///   the canonical `currentId` — i.e. the feature was referenced by a legacy id.
fn build_warning_header(feature: &ResolvedFeature, display_id: &str) -> String {
    let mut warning_header = String::new();

    if feature.metadata.deprecated == Some(true) {
        // writeln! appends '\n', matching the official's trailing newline on the
        // deprecation line (which causes the shell to echo a blank line after it).
        writeln!(
            warning_header,
            "(!) WARNING: Using the deprecated Feature \"{display_id}\". \
             This Feature will no longer receive any further updates/support."
        )
        .unwrap();
    }

    // The user's ref leaf: last `/`-segment of the version-stripped original_ref.
    // This matches `feature.id` in the official source (set to featureRef.id =
    // last path segment of the user's OCI ref), NOT our `feature.id` (on-disk dir).
    let raw = feature.original_ref.as_str();
    let user_ref_leaf = if raw.is_empty() {
        ""
    } else {
        feature_id_without_version(raw)
            .rsplit('/')
            .next()
            .unwrap_or("")
    };

    if !feature.metadata.legacy_ids.is_empty()
        && let Some(current_id) = feature.metadata.current_id.as_deref()
        && user_ref_leaf != current_id
    {
        write!(
            warning_header,
            "(!) WARNING: This feature has been renamed. \
             Please update the reference in devcontainer.json to \"{escaped}\".",
            escaped = escape_quotes_for_shell(current_id)
        )
        .unwrap();
    }

    warning_header
}

/// Generate `devcontainer-features-install.sh` wrapper script for a feature.
///
/// Produces a script matching the official `getFeatureInstallWrapperScript`
/// output, grafted onto cella's `cd /tmp/dev-container-features/<id>` directory
/// model (instead of Docker WORKDIR). The displayed `Id` field uses the
/// version-stripped `original_ref`; the `cd` path uses `feature.id` (the
/// on-disk directory name) — these are intentionally different values.
pub fn generate_wrapper_script(feature: &ResolvedFeature) -> String {
    let name = escape_quotes_for_shell(feature.metadata.name.as_deref().unwrap_or("Unknown"));
    let description =
        escape_quotes_for_shell(feature.metadata.description.as_deref().unwrap_or(""));
    let version = escape_quotes_for_shell(&feature.metadata.version);
    let documentation =
        escape_quotes_for_shell(feature.metadata.documentation_url.as_deref().unwrap_or(""));
    // Displayed id: version-stripped user ref, falling back to "Unknown".
    let display_id = {
        let raw = feature.original_ref.as_str();
        let stripped = if raw.is_empty() {
            "Unknown"
        } else {
            feature_id_without_version(raw)
        };
        escape_quotes_for_shell(stripped)
    };

    // troubleshootingMessage: leading space + doc link, only when doc non-empty.
    let troubleshooting = if feature
        .metadata
        .documentation_url
        .as_deref()
        .is_some_and(|u| !u.is_empty())
    {
        format!(
            " Look at the documentation at {documentation} for help troubleshooting this error."
        )
    } else {
        String::new()
    };

    // Build the combined warning header (deprecation + rename), matching the
    // official single-echo pattern.
    let warning_header = build_warning_header(feature, &display_id);
    let echo_warning = if warning_header.is_empty() {
        String::new()
    } else {
        format!("echo '{warning_header}'")
    };

    // Options block: indented env-var lines, shell-escaped, joined with literal newlines.
    let option_lines: Vec<String> = feature_env_lines(feature)
        .into_iter()
        .map(|l| format!("    {}", escape_quotes_for_shell(&l)))
        .collect();
    let options_indented = if option_lines.is_empty() {
        String::new()
    } else {
        option_lines.join("\n")
    };

    // The `cd` path uses the canonical feature.id (on-disk dir), NOT original_ref.
    let dir_id = &feature.id;

    format!(
        "#!/bin/sh\n\
         set -e\n\
         \n\
         on_exit () {{\n\
         \t[ $? -eq 0 ] && exit\n\
         \techo 'ERROR: Feature \"{name}\" ({display_id}) failed to install!{troubleshooting}'\n\
         }}\n\
         \n\
         trap on_exit EXIT\n\
         \n\
         echo ===========================================================================\n\
         {echo_warning}\n\
         echo 'Feature       : {name}'\n\
         echo 'Description   : {description}'\n\
         echo 'Id            : {display_id}'\n\
         echo 'Version       : {version}'\n\
         echo 'Documentation : {documentation}'\n\
         echo 'Options       :'\n\
         echo '{options_indented}'\n\
         echo ===========================================================================\n\
         \n\
         cd /tmp/dev-container-features/{dir_id}\n\
         set -a\n\
         . ../devcontainer-features.builtin.env\n\
         . ./devcontainer-features.env\n\
         set +a\n\
         \n\
         chmod +x ./install.sh\n\
         ./install.sh\n"
    )
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

/// Convert a feature option id to its install-time env var name, matching the
/// official CLI's `getSafeId`: every non-`[A-Za-z0-9_]` char becomes `_`, a
/// leading run of digits/underscores collapses to a single `_`, then the whole
/// thing is uppercased (e.g. `my-option` → `MY_OPTION`, `node.version` →
/// `NODE_VERSION`, `123bad` → `_BAD`). A bare `to_uppercase()` would emit
/// invalid shell names like `MY-OPTION`, which `install.sh` would never see.
pub(crate) fn option_key_to_env_var(key: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let first_valid = sanitized
        .chars()
        .position(|c| c != '_' && !c.is_ascii_digit())
        .unwrap_or(sanitized.len());
    let normalized = if first_valid > 0 {
        format!("_{}", &sanitized[first_valid..])
    } else {
        sanitized
    };
    normalized.to_uppercase()
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
            oci: None,
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
            "vscode",
            "vscode",
            &features,
            false,
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
            "vscode",
            "vscode",
            &features,
            false,
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

        let result = generate_dockerfile("ubuntu:22.04", "root", "root", "root", &features, false);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Feature with containerUser — all installs run as root regardless
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

        let result = generate_dockerfile(
            "ubuntu:22.04",
            "vscode",
            "vscode",
            "vscode",
            &features,
            false,
        );
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // USER reset at end via build arg
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

        let result = generate_dockerfile(
            "ubuntu:22.04",
            "vscode",
            "vscode",
            "vscode",
            &features,
            false,
        );
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

        let result = generate_dockerfile("ubuntu:22.04", "root", "root", "root", &features, false);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // USER root always emitted for installable features
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

        // Image user is root — USER root is still emitted (matches original CLI).
        let result = generate_dockerfile("ubuntu:22.04", "root", "root", "root", &features, false);
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

        let result = generate_dockerfile(
            "ubuntu:22.04",
            "vscode",
            "vscode",
            "vscode",
            &features,
            false,
        );
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // All features install as root — no per-feature USER transitions
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

        let result = generate_dockerfile(
            "ubuntu:22.04",
            "vscode",
            "vscode",
            "vscode",
            &features,
            false,
        );
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

        let result = generate_dockerfile("ubuntu:22.04", "root", "root", "root", &features, false);
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
        let dockerfile =
            generate_dockerfile("ubuntu:22.04", "root", "root", "root", &features, false);
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

        let result = generate_dockerfile("ubuntu:22.04", "root", "root", "root", &features, false);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Empty features list
    // ---------------------------------------------------------------

    #[test]
    fn empty_features_list() {
        let result = generate_dockerfile("ubuntu:22.04", "vscode", "vscode", "vscode", &[], false);
        insta::assert_snapshot!(result);
    }

    // ---------------------------------------------------------------
    // Wrapper script content
    // ---------------------------------------------------------------

    #[test]
    fn wrapper_script_content() {
        let mut options = HashMap::new();
        options.insert("version".to_string(), make_option(serde_json::json!("lts")));
        options.insert(
            "nodeGypDependencies".to_string(),
            make_option(serde_json::json!(true)),
        );
        let mut user_options = HashMap::new();
        user_options.insert("version".to_string(), serde_json::json!("18"));

        let feature = make_feature(
            "node",
            "ghcr.io/devcontainers/features/node:1",
            true,
            user_options,
            FeatureMetadata {
                id: "node".to_string(),
                version: "2.1.0".to_string(),
                name: Some("Node.js".to_string()),
                description: Some("Installs Node.js".to_string()),
                documentation_url: Some("https://example.com/node".to_string()),
                options,
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        insta::assert_snapshot!(script);
    }

    #[test]
    fn wrapper_script_deprecated_feature() {
        let feature = make_feature(
            "node",
            "ghcr.io/devcontainers/features/node:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "node".to_string(),
                version: "2.1.0".to_string(),
                name: Some("Node.js".to_string()),
                description: Some("Installs Node.js".to_string()),
                documentation_url: Some("https://example.com/node".to_string()),
                deprecated: Some(true),
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        assert!(
            script.contains("(!) WARNING: Using the deprecated Feature"),
            "deprecation warning missing:\n{script}"
        );
        insta::assert_snapshot!(script);
    }

    #[test]
    fn wrapper_script_shell_escape() {
        // A value with an apostrophe must be escaped so the shell script is valid.
        let feature = make_feature(
            "my-tool",
            "ghcr.io/example/my-tool:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "my-tool".to_string(),
                version: "1.0.0".to_string(),
                name: Some("Eve's Tool".to_string()),
                description: Some("A user's best friend".to_string()),
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        // Apostrophe in name should be shell-escaped as '\''
        assert!(
            script.contains("Eve'\\''s Tool"),
            "name apostrophe not escaped:\n{script}"
        );
        assert!(
            script.contains("A user'\\''s best friend"),
            "description apostrophe not escaped:\n{script}"
        );
    }

    // ---------------------------------------------------------------
    // Rename warning (referenced by legacy id)
    // ---------------------------------------------------------------

    #[test]
    fn wrapper_script_renamed_feature_emits_warning() {
        // User referenced `maven:1`; canonical id is `java` — rename warning fires.
        let feature = make_feature(
            "java",
            "ghcr.io/devcontainers/features/maven:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "java".to_string(),
                version: "1.0.0".to_string(),
                name: Some("Java".to_string()),
                legacy_ids: vec!["maven".to_string(), "gradle".to_string()],
                current_id: Some("java".to_string()),
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        assert!(
            script.contains("(!) WARNING: This feature has been renamed."),
            "rename warning missing:\n{script}"
        );
        assert!(
            script.contains("devcontainer.json to \"java\""),
            "rename warning should mention canonical id:\n{script}"
        );
        assert!(
            !script.contains("(!) WARNING: Using the deprecated Feature"),
            "should not have deprecation warning:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_canonical_ref_no_rename_warning() {
        // User referenced `java:1`; leaf matches current_id — no rename warning.
        let feature = make_feature(
            "java",
            "ghcr.io/devcontainers/features/java:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "java".to_string(),
                version: "1.0.0".to_string(),
                name: Some("Java".to_string()),
                legacy_ids: vec!["maven".to_string(), "gradle".to_string()],
                current_id: Some("java".to_string()),
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        assert!(
            !script.contains("(!) WARNING: This feature has been renamed."),
            "should not emit rename warning for canonical ref:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_legacy_ids_without_current_id_no_rename_warning() {
        // legacyIds present but currentId absent — no rename warning.
        let feature = make_feature(
            "java",
            "ghcr.io/devcontainers/features/maven:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "java".to_string(),
                version: "1.0.0".to_string(),
                name: Some("Java".to_string()),
                legacy_ids: vec!["maven".to_string()],
                current_id: None,
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        assert!(
            !script.contains("(!) WARNING: This feature has been renamed."),
            "should not emit rename warning without currentId:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_deprecated_and_renamed() {
        // Both deprecated AND referenced by legacy id — both warnings in one echo.
        let feature = make_feature(
            "java",
            "ghcr.io/devcontainers/features/maven:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "java".to_string(),
                version: "1.0.0".to_string(),
                name: Some("Java".to_string()),
                legacy_ids: vec!["maven".to_string()],
                current_id: Some("java".to_string()),
                deprecated: Some(true),
                ..Default::default()
            },
        );

        let script = generate_wrapper_script(&feature);
        assert!(
            script.contains("(!) WARNING: Using the deprecated Feature"),
            "deprecation warning missing:\n{script}"
        );
        assert!(
            script.contains("(!) WARNING: This feature has been renamed."),
            "rename warning missing:\n{script}"
        );
        // Both must be inside a single echo (the echo appears exactly once).
        let echo_count = script.matches("echo '(!)").count();
        assert_eq!(
            echo_count, 1,
            "both warnings must share one echo: {echo_count} found"
        );
    }

    // ---------------------------------------------------------------
    // Builtin env content
    // ---------------------------------------------------------------

    #[test]
    fn builtin_env_content() {
        let env = generate_builtin_env("vscode", "vscode");
        insta::assert_snapshot!(env);
    }

    #[test]
    fn builtin_env_different_users() {
        let env = generate_builtin_env("developer", "admin");
        insta::assert_snapshot!(env);
    }

    // ---------------------------------------------------------------
    // Feature env with options
    // ---------------------------------------------------------------

    #[test]
    fn feature_env_with_options() {
        let mut options = HashMap::new();
        options.insert("version".to_string(), make_option(serde_json::json!("lts")));
        options.insert(
            "nodeGypDependencies".to_string(),
            make_option(serde_json::json!(true)),
        );

        let mut user_options = HashMap::new();
        user_options.insert("version".to_string(), serde_json::json!("18"));

        let feature = make_feature(
            "node",
            "ghcr.io/devcontainers/features/node:1",
            true,
            user_options,
            FeatureMetadata {
                id: "node".to_string(),
                options,
                ..Default::default()
            },
        );

        let env = generate_feature_env(&feature);
        insta::assert_snapshot!(env);
    }

    #[test]
    fn feature_env_no_options() {
        let feature = make_feature(
            "minimal",
            "ghcr.io/example/minimal:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "minimal".to_string(),
                ..Default::default()
            },
        );

        let env = generate_feature_env(&feature);
        assert!(env.is_empty());
    }

    // --- Spec compliance: option id → env var name (getSafeId) ---
    // Reference: https://containers.dev/implementors/spec/#dev-container-features

    #[test]
    fn option_key_to_env_var_simple_uppercase() {
        assert_eq!(option_key_to_env_var("version"), "VERSION");
    }

    #[test]
    fn option_key_to_env_var_hyphens() {
        assert_eq!(option_key_to_env_var("my-option"), "MY_OPTION");
    }

    #[test]
    fn option_key_to_env_var_dots() {
        assert_eq!(option_key_to_env_var("node.version"), "NODE_VERSION");
    }

    #[test]
    fn option_key_to_env_var_leading_digit() {
        assert_eq!(option_key_to_env_var("123bad"), "_BAD");
    }

    #[test]
    fn option_key_to_env_var_special_chars() {
        assert_eq!(option_key_to_env_var("my@option#1"), "MY_OPTION_1");
    }

    #[test]
    fn feature_env_sanitizes_option_key_for_install_sh() {
        // A hyphenated option key must become a valid shell var name so
        // install.sh actually receives it.
        let mut options = HashMap::new();
        options.insert(
            "install-zsh".to_string(),
            make_option(serde_json::json!(true)),
        );
        let feature = make_feature(
            "common",
            "ghcr.io/devcontainers/features/common-utils:2",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "common".to_string(),
                options,
                ..Default::default()
            },
        );
        let env = generate_feature_env(&feature);
        assert!(env.contains("INSTALL_ZSH=\"true\""), "got: {env}");
        assert!(!env.contains("INSTALL-ZSH"), "invalid shell name: {env}");
    }

    #[test]
    fn feature_env_omits_option_without_default() {
        // An option with no declared default (Null) and no user value must be
        // omitted entirely (matches official `'default' in options`), not
        // emitted as KEY="".
        let mut options = HashMap::new();
        options.insert("opt".to_string(), make_option(serde_json::Value::Null));
        let feature = make_feature(
            "f",
            "ghcr.io/example/f:1",
            true,
            HashMap::new(),
            FeatureMetadata {
                id: "f".to_string(),
                options,
                ..Default::default()
            },
        );
        let env = generate_feature_env(&feature);
        assert!(
            !env.contains("OPT="),
            "no-default option must be omitted: {env}"
        );
    }
}
