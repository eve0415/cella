//! Typed resolution of `docker compose config` output.
//!
//! Runs `docker compose config --format json` to get the fully resolved
//! compose configuration (with variable substitution, extends, merging),
//! then deserializes the primary service's build/image info.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::CellaComposeError;

/// Resolved compose config from `docker compose config --format json`.
#[derive(Debug, Deserialize, Default)]
pub struct ResolvedComposeConfig {
    /// Map of service name to resolved service definition.
    #[serde(default)]
    pub services: HashMap<String, ResolvedService>,
    /// Top-level volumes section from the resolved compose config.
    ///
    /// Keys are volume names. Values are raw JSON (`driver`, `driver_opts`,
    /// `external`, etc.). Kept as `serde_json::Value` to preserve all
    /// driver options without needing an exhaustive schema.
    #[serde(default)]
    pub volumes: HashMap<String, serde_json::Value>,
}

/// A single resolved service from the compose config.
#[derive(Debug, Deserialize, Default)]
pub struct ResolvedService {
    /// Pre-built image reference (e.g., `node:18`).
    pub image: Option<String>,
    /// Build configuration (Dockerfile-based).
    pub build: Option<ResolvedBuild>,
    /// Resolved volumes list. Each entry is either a short-form string
    /// (`"./host:/container[:opts]"`) or a long-form object
    /// (`{"type": "bind", "source": ..., "target": ...}`). Kept as raw
    /// `serde_json::Value` because both forms are valid in the compose spec.
    #[serde(default)]
    pub volumes: Vec<serde_json::Value>,
    /// Raw `volumes_from` entries. Each entry may be a string (`"service"`,
    /// `"service:ro"`) or an object (`{"source": "service"}`). Kept as raw
    /// `serde_json::Value` to avoid an exhaustive schema; non-empty signals
    /// that this service inherits volumes from another service at runtime.
    #[serde(default)]
    pub volumes_from: Vec<serde_json::Value>,
    /// Service dependencies. May be absent (→ `Value::Null`), an array of
    /// service name strings (short form), or an object mapping service name
    /// to condition (long form). Kept as raw `serde_json::Value` to handle
    /// both compose forms without an exhaustive schema.
    #[serde(default)]
    pub depends_on: serde_json::Value,
    /// Tmpfs mounts declared via the top-level `tmpfs:` service key.
    /// May be a single string or an array of strings.
    #[serde(default)]
    pub tmpfs: serde_json::Value,
    /// Config file mounts. Each entry is typically
    /// `{source, target, uid, gid, mode}` (long form) or a plain string
    /// name (short form). Kept as raw `serde_json::Value` to handle both.
    #[serde(default)]
    pub configs: Vec<serde_json::Value>,
    /// Secret file mounts. Same shape as `configs`.
    #[serde(default)]
    pub secrets: Vec<serde_json::Value>,
    /// Service `entrypoint`. May be absent (→ `Value::Null`), a string (shell
    /// form, e.g. `"/docker-entrypoint.sh foo"`), or an array of strings (exec
    /// form). Kept as raw `serde_json::Value` and normalized by
    /// [`extract_service_entrypoint_command`]; the compose override needs it to
    /// preserve the service's original entrypoint when wrapping for feature
    /// entrypoints.
    #[serde(default)]
    pub entrypoint: serde_json::Value,
    /// Service `command`. Same string/array/absent shape as `entrypoint`.
    #[serde(default)]
    pub command: serde_json::Value,
}

/// Resolved build config for a compose service.
#[derive(Debug, Deserialize)]
pub struct ResolvedBuild {
    /// Build context directory (absolute path after resolution).
    pub context: Option<String>,
    /// Path to the Dockerfile (absolute or relative to context).
    pub dockerfile: Option<String>,
    /// Multi-stage build target.
    pub target: Option<String>,
    /// Build arguments.
    #[serde(default)]
    pub args: HashMap<String, String>,
}

/// Extracted build information for a compose service.
#[derive(Debug)]
pub enum ServiceBuildInfo {
    /// Service uses a pre-built image.
    Image {
        /// The image reference (e.g., `node:18`).
        image: String,
    },
    /// Service builds from a Dockerfile.
    Build {
        /// Absolute path to the build context directory.
        context: PathBuf,
        /// Dockerfile filename (default: `Dockerfile`).
        dockerfile: String,
        /// Optional multi-stage build target.
        target: Option<String>,
        /// Build arguments.
        args: HashMap<String, String>,
        /// Explicit `image:` reference when the service declares both `build:`
        /// and `image:`. Docker Compose tags the build output with this name
        /// instead of the default `{project}-{service}`.
        image: Option<String>,
    },
}

impl ServiceBuildInfo {
    /// The image name the primary service resolves to after `docker compose build`.
    ///
    /// For an image-only service this is the image reference itself. For a
    /// build-based service it is the explicit `image:` reference if one is
    /// declared (Docker Compose tags the build output with it), otherwise
    /// `{project}-{service}`, the default name Compose assigns.
    #[must_use]
    pub fn resolved_image_name(&self, project_name: &str, service: &str) -> String {
        match self {
            Self::Image { image }
            | Self::Build {
                image: Some(image), ..
            } => image.clone(),
            Self::Build { image: None, .. } => format!("{project_name}-{service}"),
        }
    }
}

/// Extract the build/image info for a specific service from the resolved compose config.
///
/// When a service has both `build` and `image`, `build` takes precedence
/// (matching Docker Compose behavior).
///
/// # Errors
///
/// Returns an error if the service is not found or has neither `build` nor `image`.
pub fn extract_service_build_info(
    config: &ResolvedComposeConfig,
    service: &str,
) -> Result<ServiceBuildInfo, CellaComposeError> {
    let svc = config
        .services
        .get(service)
        .ok_or_else(|| CellaComposeError::ServiceNotFound {
            service: service.to_string(),
            available: config
                .services
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        })?;

    // build takes precedence over image (Docker Compose semantics)
    if let Some(ref build) = svc.build {
        let context = build.context.as_deref().unwrap_or(".").to_string();
        let dockerfile = build
            .dockerfile
            .clone()
            .unwrap_or_else(|| "Dockerfile".to_string());

        return Ok(ServiceBuildInfo::Build {
            context: PathBuf::from(context),
            dockerfile,
            target: build.target.clone(),
            args: build.args.clone(),
            // A co-present `image:` is the tag Compose applies to the build output.
            image: svc.image.clone(),
        });
    }

    if let Some(ref image) = svc.image {
        return Ok(ServiceBuildInfo::Image {
            image: image.clone(),
        });
    }

    Err(CellaComposeError::ServiceHasNoBuildOrImage {
        service: service.to_string(),
    })
}

/// A service's resolved `(entrypoint, command)` argv token lists.
///
/// Each is `None` when the service does not declare that key, and `Some(tokens)`
/// otherwise (string forms shell-split, array forms verbatim). `Some(empty)`
/// means an explicit empty list (`command: []`), which is distinct from absent.
pub type ServiceEntrypointCommand = (Option<Vec<String>>, Option<Vec<String>>);

/// Normalize a compose `entrypoint`/`command` value into argv tokens.
///
/// Mirrors the official CLI's `typeof x === 'string' ? shellQuote.parse(x) : x`:
/// - **String** → POSIX shell word-split (so `"sh -c 'echo hi'"` becomes three
///   tokens). A string that fails to parse (e.g. an unbalanced quote, which a
///   resolved compose config should never produce) is kept as a single token
///   rather than dropped.
/// - **Array** → its string elements verbatim (non-string elements are skipped).
/// - **Null / anything else** → `None` (the key was absent).
fn compose_value_to_words(value: &serde_json::Value) -> Option<Vec<String>> {
    match value {
        serde_json::Value::String(s) => {
            Some(shell_words::split(s).unwrap_or_else(|_| vec![s.clone()]))
        }
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect(),
        ),
        _ => None,
    }
}

/// Extract a service's resolved `entrypoint` and `command` as argv token lists.
///
/// Returns `(entrypoint, command)` where each is `None` when the service does
/// not declare that key and `Some(tokens)` otherwise (string forms are
/// shell-split, array forms taken as-is). Used by the compose override to
/// resolve the wrapped entrypoint's `userEntrypoint`/`userCommand` the same way
/// the official CLI does.
///
/// # Errors
///
/// Returns an error if the service is not present in the resolved config.
pub fn extract_service_entrypoint_command(
    config: &ResolvedComposeConfig,
    service: &str,
) -> Result<ServiceEntrypointCommand, CellaComposeError> {
    let svc = config
        .services
        .get(service)
        .ok_or_else(|| CellaComposeError::ServiceNotFound {
            service: service.to_string(),
            available: config
                .services
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        })?;
    Ok((
        compose_value_to_words(&svc.entrypoint),
        compose_value_to_words(&svc.command),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_with_image() {
        let json = r#"{
            "services": {
                "app": {
                    "image": "node:18"
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let info = extract_service_build_info(&config, "app").unwrap();
        assert!(matches!(info, ServiceBuildInfo::Image { image } if image == "node:18"));
    }

    #[test]
    fn parse_config_with_build() {
        let json = r#"{
            "services": {
                "app": {
                    "build": {
                        "context": "/workspace",
                        "dockerfile": "Dockerfile.dev",
                        "target": "development",
                        "args": {
                            "NODE_VERSION": "18"
                        }
                    }
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let info = extract_service_build_info(&config, "app").unwrap();
        match info {
            ServiceBuildInfo::Build {
                context,
                dockerfile,
                target,
                args,
                ..
            } => {
                assert_eq!(context, PathBuf::from("/workspace"));
                assert_eq!(dockerfile, "Dockerfile.dev");
                assert_eq!(target.as_deref(), Some("development"));
                assert_eq!(args.get("NODE_VERSION").map(String::as_str), Some("18"));
            }
            ServiceBuildInfo::Image { .. } => panic!("expected Build variant"),
        }
    }

    #[test]
    fn build_takes_precedence_over_image() {
        let json = r#"{
            "services": {
                "app": {
                    "image": "myapp:latest",
                    "build": {
                        "context": "/workspace"
                    }
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let info = extract_service_build_info(&config, "app").unwrap();
        // `build` takes precedence (cella still builds), but the co-present
        // `image:` is captured — Docker Compose tags the build output with it, so
        // it's the resolved image name rather than the `{project}-{service}` default.
        assert!(
            matches!(&info, ServiceBuildInfo::Build { image: Some(img), .. } if img == "myapp:latest")
        );
        assert_eq!(info.resolved_image_name("proj", "app"), "myapp:latest");
    }

    #[test]
    fn service_not_found() {
        let json = r#"{ "services": { "app": { "image": "node:18" } } }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let err = extract_service_build_info(&config, "web").unwrap_err();
        assert!(matches!(err, CellaComposeError::ServiceNotFound { .. }));
    }

    #[test]
    fn service_no_build_or_image() {
        let json = r#"{ "services": { "app": {} } }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let err = extract_service_build_info(&config, "app").unwrap_err();
        assert!(matches!(
            err,
            CellaComposeError::ServiceHasNoBuildOrImage { .. }
        ));
    }

    #[test]
    fn resolved_image_name_uses_image_reference() {
        let info = ServiceBuildInfo::Image {
            image: "node:20".to_string(),
        };
        assert_eq!(info.resolved_image_name("myproj", "app"), "node:20");
    }

    #[test]
    fn resolved_image_name_uses_project_service_for_build() {
        let info = ServiceBuildInfo::Build {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".to_string(),
            target: None,
            args: HashMap::new(),
            image: None,
        };
        assert_eq!(info.resolved_image_name("myproj", "app"), "myproj-app");
    }

    #[test]
    fn build_defaults() {
        let json = r#"{
            "services": {
                "app": {
                    "build": {}
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let info = extract_service_build_info(&config, "app").unwrap();
        match info {
            ServiceBuildInfo::Build {
                context,
                dockerfile,
                target,
                args,
                ..
            } => {
                assert_eq!(context, PathBuf::from("."));
                assert_eq!(dockerfile, "Dockerfile");
                assert!(target.is_none());
                assert!(args.is_empty());
            }
            ServiceBuildInfo::Image { .. } => panic!("expected Build variant"),
        }
    }

    #[test]
    fn empty_services_map() {
        let json = r#"{"services": {}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let err = extract_service_build_info(&config, "app").unwrap_err();
        assert!(matches!(err, CellaComposeError::ServiceNotFound { .. }));
    }

    #[test]
    fn build_with_empty_args() {
        let json = r#"{
            "services": {
                "app": {
                    "build": {
                        "context": "/workspace",
                        "args": {}
                    }
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let info = extract_service_build_info(&config, "app").unwrap();
        match info {
            ServiceBuildInfo::Build { context, args, .. } => {
                assert_eq!(context, PathBuf::from("/workspace"));
                assert!(args.is_empty());
            }
            ServiceBuildInfo::Image { .. } => panic!("expected Build variant"),
        }
    }

    #[test]
    fn multiple_services_correct_one_selected() {
        let json = r#"{
            "services": {
                "web": { "image": "nginx:latest" },
                "api": { "image": "node:20" },
                "db": { "image": "postgres:16" }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let info = extract_service_build_info(&config, "api").unwrap();
        assert!(matches!(info, ServiceBuildInfo::Image { image } if image == "node:20"));
    }

    #[test]
    fn deserialize_volumes_short_and_long_form() {
        let json = r#"{
            "services": {
                "app": {
                    "volumes": [
                        "/host:/container:ro",
                        {"type": "bind", "source": "/h", "target": "/t"}
                    ]
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert_eq!(svc.volumes.len(), 2);
    }

    #[test]
    fn deserialize_service_without_volumes_yields_empty_vec() {
        let json = r#"{"services": {"app": {"image": "nginx"}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert!(svc.volumes.is_empty());
    }

    #[test]
    fn service_not_found_lists_available() {
        let json = r#"{
            "services": {
                "alpha": { "image": "a:1" },
                "beta": { "image": "b:2" }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let err = extract_service_build_info(&config, "gamma").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("gamma"),
            "error should mention missing service"
        );
        assert!(
            msg.contains("alpha"),
            "error should list available service alpha"
        );
        assert!(
            msg.contains("beta"),
            "error should list available service beta"
        );
    }

    #[test]
    fn deserialize_top_level_volumes() {
        let json = r#"{"volumes": {"mycache": {"external": true}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        assert!(
            config.volumes.contains_key("mycache"),
            "top-level volumes must deserialize"
        );
    }

    #[test]
    fn deserialize_volumes_from() {
        let json = r#"{"services": {"app": {"volumes_from": ["db", {"source": "cache"}]}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert_eq!(svc.volumes_from.len(), 2);
    }

    #[test]
    fn deserialize_service_without_volumes_from_yields_empty_vec() {
        let json = r#"{"services": {"app": {"image": "nginx"}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert!(svc.volumes_from.is_empty());
    }

    #[test]
    fn deserialize_depends_on_short_form() {
        let json = r#"{"services": {"app": {"depends_on": ["db", "cache"]}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert!(svc.depends_on.is_array());
    }

    #[test]
    fn deserialize_depends_on_long_form() {
        let json =
            r#"{"services": {"app": {"depends_on": {"db": {"condition": "service_started"}}}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert!(svc.depends_on.is_object());
    }

    #[test]
    fn deserialize_depends_on_absent_defaults_to_null() {
        let json = r#"{"services": {"app": {}}}"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert!(svc.depends_on.is_null());
    }

    #[test]
    fn deserialize_tmpfs_and_configs_and_secrets() {
        let json = r#"{
            "services": {
                "app": {
                    "tmpfs": ["/run", "/var/tmp"],
                    "configs": [{"source": "cfg", "target": "/etc/cfg"}],
                    "secrets": ["secret-a"]
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let svc = config.services.get("app").unwrap();
        assert!(svc.tmpfs.is_array());
        assert_eq!(svc.configs.len(), 1);
        assert_eq!(svc.secrets.len(), 1);
    }

    // -----------------------------------------------------------------------
    // extract_service_entrypoint_command (string / array / absent)
    // -----------------------------------------------------------------------

    #[test]
    fn entrypoint_command_string_form_is_shell_split() {
        // Compose string form is shell-split (matching shellQuote.parse): the
        // quoted segment stays a single token.
        let json = r#"{
            "services": {
                "app": {
                    "entrypoint": "/docker-entrypoint.sh",
                    "command": "sh -c 'echo hi there'"
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let (ep, cmd) = extract_service_entrypoint_command(&config, "app").unwrap();
        assert_eq!(ep, Some(vec!["/docker-entrypoint.sh".to_string()]));
        assert_eq!(
            cmd,
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hi there".to_string(),
            ])
        );
    }

    #[test]
    fn entrypoint_command_array_form_is_verbatim() {
        // Exec (array) form is taken element-for-element, no splitting.
        let json = r#"{
            "services": {
                "app": {
                    "entrypoint": ["/bin/tini", "--"],
                    "command": ["node", "server.js", "--port=3000"]
                }
            }
        }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let (ep, cmd) = extract_service_entrypoint_command(&config, "app").unwrap();
        assert_eq!(ep, Some(vec!["/bin/tini".to_string(), "--".to_string()]));
        assert_eq!(
            cmd,
            Some(vec![
                "node".to_string(),
                "server.js".to_string(),
                "--port=3000".to_string(),
            ])
        );
    }

    #[test]
    fn entrypoint_command_absent_yields_none() {
        // No keys declared -> both None (so the override falls back to image
        // entrypoint/cmd, mirroring the official `composeEntrypoint || image`).
        let json = r#"{ "services": { "app": { "image": "nginx" } } }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let (ep, cmd) = extract_service_entrypoint_command(&config, "app").unwrap();
        assert_eq!(ep, None);
        assert_eq!(cmd, None);
    }

    #[test]
    fn entrypoint_command_empty_array_is_some_empty() {
        // An explicit empty array (`command: []`) is distinct from absent: it
        // resolves to Some(empty), which the override must honor (it clears the
        // image CMD) rather than treat as "fall back to image".
        let json = r#"{ "services": { "app": { "command": [] } } }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let (ep, cmd) = extract_service_entrypoint_command(&config, "app").unwrap();
        assert_eq!(ep, None);
        assert_eq!(cmd, Some(Vec::new()));
    }

    #[test]
    fn entrypoint_command_service_not_found_errors() {
        let json = r#"{ "services": { "app": { "image": "nginx" } } }"#;
        let config: ResolvedComposeConfig = serde_json::from_str(json).unwrap();
        let err = extract_service_entrypoint_command(&config, "web").unwrap_err();
        assert!(matches!(err, CellaComposeError::ServiceNotFound { .. }));
    }
}
