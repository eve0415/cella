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
#[derive(Debug, Deserialize)]
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
    },
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
        assert!(matches!(info, ServiceBuildInfo::Build { .. }));
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
}
