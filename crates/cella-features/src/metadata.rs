//! Parser for `devcontainer-feature.json` metadata files.

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::FeatureError;
use crate::types::{FeatureMetadata, FeatureOption, OptionType};

/// Deserializes `devcontainer-feature.json` content into [`FeatureMetadata`].
///
/// # Errors
///
/// Returns [`FeatureError::InvalidMetadata`] if the JSON is malformed or
/// missing required fields (`id`, `version`).
pub fn parse_feature_metadata(json: &str) -> Result<FeatureMetadata, FeatureError> {
    let dto: FeatureMetadataDto = serde_json::from_str(json).map_err(|e| {
        // Try to extract the id from a partial parse for a better error message.
        let feature_id = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(String::from))
            .unwrap_or_else(|| "<unknown>".to_string());
        FeatureError::InvalidMetadata {
            feature_id,
            reason: e.to_string(),
        }
    })?;

    dto.into_metadata()
}

/// Internal DTO that maps camelCase JSON field names to Rust fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeatureMetadataDto {
    id: String,
    version: String,
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    options: HashMap<String, FeatureOptionDto>,
    #[serde(default)]
    installs_after: Vec<String>,
    container_user: Option<String>,
    entrypoint: Option<String>,
    #[serde(default)]
    mounts: Vec<String>,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    security_opt: Vec<String>,
    privileged: Option<bool>,
    init: Option<bool>,
    #[serde(default)]
    container_env: HashMap<String, String>,
    customizations: Option<serde_json::Value>,
    on_create_command: Option<serde_json::Value>,
    post_create_command: Option<serde_json::Value>,
    post_start_command: Option<serde_json::Value>,
    post_attach_command: Option<serde_json::Value>,
    #[serde(default)]
    legacy_ids: Vec<String>,
    deprecated: Option<bool>,
}

/// Internal DTO for a single feature option.
#[derive(Deserialize)]
struct FeatureOptionDto {
    #[serde(rename = "type")]
    option_type: String,
    #[serde(default)]
    default: serde_json::Value,
    description: Option<String>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<String>>,
}

impl FeatureMetadataDto {
    fn into_metadata(self) -> Result<FeatureMetadata, FeatureError> {
        let mut options = HashMap::with_capacity(self.options.len());

        for (key, dto) in self.options {
            let option_type = match dto.option_type.as_str() {
                "string" => OptionType::String,
                "boolean" => OptionType::Boolean,
                other => {
                    return Err(FeatureError::InvalidMetadata {
                        feature_id: self.id,
                        reason: format!("unknown option type \"{other}\" for option \"{key}\""),
                    });
                }
            };

            options.insert(
                key,
                FeatureOption {
                    option_type,
                    default: dto.default,
                    description: dto.description,
                    enum_values: dto.enum_values,
                },
            );
        }

        Ok(FeatureMetadata {
            id: self.id,
            version: self.version,
            name: self.name,
            description: self.description,
            options,
            installs_after: self.installs_after,
            container_user: self.container_user,
            entrypoint: self.entrypoint,
            mounts: self.mounts,
            cap_add: self.cap_add,
            security_opt: self.security_opt,
            privileged: self.privileged,
            init: self.init,
            container_env: self.container_env,
            customizations: self.customizations,
            on_create_command: self.on_create_command,
            post_create_command: self.post_create_command,
            post_start_command: self.post_start_command,
            post_attach_command: self.post_attach_command,
            legacy_ids: self.legacy_ids,
            deprecated: self.deprecated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_valid_metadata() {
        let json = r#"{"id": "node", "version": "1.0.0"}"#;
        let meta = parse_feature_metadata(json).expect("should parse minimal metadata");
        assert_eq!(meta.id, "node");
        assert_eq!(meta.version, "1.0.0");
        assert!(meta.name.is_none());
        assert!(meta.description.is_none());
        assert!(meta.options.is_empty());
        assert!(meta.installs_after.is_empty());
        assert!(meta.container_user.is_none());
        assert!(meta.entrypoint.is_none());
        assert!(meta.mounts.is_empty());
        assert!(meta.cap_add.is_empty());
        assert!(meta.security_opt.is_empty());
        assert!(meta.privileged.is_none());
        assert!(meta.init.is_none());
        assert!(meta.container_env.is_empty());
        assert!(meta.customizations.is_none());
        assert!(meta.on_create_command.is_none());
        assert!(meta.post_create_command.is_none());
        assert!(meta.post_start_command.is_none());
        assert!(meta.post_attach_command.is_none());
        assert!(meta.legacy_ids.is_empty());
        assert!(meta.deprecated.is_none());
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn full_metadata() {
        let json = r#"{
            "id": "node",
            "version": "2.1.0",
            "name": "Node.js",
            "description": "Installs Node.js and npm",
            "options": {
                "version": {
                    "type": "string",
                    "default": "lts",
                    "description": "Node.js version",
                    "enum": ["lts", "20", "18", "16"]
                },
                "installYarn": {
                    "type": "boolean",
                    "default": true,
                    "description": "Install Yarn package manager"
                }
            },
            "installsAfter": ["ghcr.io/devcontainers/features/common-utils"],
            "containerUser": "node",
            "entrypoint": "/usr/local/share/nvm-entrypoint.sh",
            "mounts": ["source=node-modules,target=/usr/local/lib/node_modules,type=volume"],
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "privileged": false,
            "init": true,
            "containerEnv": {
                "NVM_DIR": "/usr/local/share/nvm",
                "PATH": "/usr/local/share/nvm/current/bin:${PATH}"
            },
            "customizations": {"vscode": {"extensions": ["dbaeumer.vscode-eslint"]}},
            "onCreateCommand": "npm install",
            "postCreateCommand": {"setup": "npm run setup"},
            "postStartCommand": "npm start",
            "postAttachCommand": "echo attached",
            "legacyIds": ["nodejs", "node-js"],
            "deprecated": false
        }"#;

        let meta = parse_feature_metadata(json).expect("should parse full metadata");
        assert_eq!(meta.id, "node");
        assert_eq!(meta.version, "2.1.0");
        assert_eq!(meta.name.as_deref(), Some("Node.js"));
        assert_eq!(
            meta.description.as_deref(),
            Some("Installs Node.js and npm")
        );

        // Options
        assert_eq!(meta.options.len(), 2);

        let version_opt = &meta.options["version"];
        assert_eq!(version_opt.option_type, OptionType::String);
        assert_eq!(version_opt.default, serde_json::json!("lts"));
        assert_eq!(version_opt.description.as_deref(), Some("Node.js version"));
        assert_eq!(
            version_opt.enum_values.as_deref(),
            Some(&["lts", "20", "18", "16"].map(String::from)[..])
        );

        let yarn_opt = &meta.options["installYarn"];
        assert_eq!(yarn_opt.option_type, OptionType::Boolean);
        assert_eq!(yarn_opt.default, serde_json::json!(true));
        assert!(yarn_opt.enum_values.is_none());

        // Other fields
        assert_eq!(
            meta.installs_after,
            vec!["ghcr.io/devcontainers/features/common-utils"]
        );
        assert_eq!(meta.container_user.as_deref(), Some("node"));
        assert_eq!(
            meta.entrypoint.as_deref(),
            Some("/usr/local/share/nvm-entrypoint.sh")
        );
        assert_eq!(
            meta.mounts,
            vec!["source=node-modules,target=/usr/local/lib/node_modules,type=volume"]
        );
        assert_eq!(meta.cap_add, vec!["SYS_PTRACE"]);
        assert_eq!(meta.security_opt, vec!["seccomp=unconfined"]);
        assert_eq!(meta.privileged, Some(false));
        assert_eq!(meta.init, Some(true));
        assert_eq!(meta.container_env.len(), 2);
        assert_eq!(meta.container_env["NVM_DIR"], "/usr/local/share/nvm");
        assert!(meta.customizations.is_some());
        assert!(meta.on_create_command.is_some());
        assert!(meta.post_create_command.is_some());
        assert!(meta.post_start_command.is_some());
        assert!(meta.post_attach_command.is_some());
        assert_eq!(meta.legacy_ids, vec!["nodejs", "node-js"]);
        assert_eq!(meta.deprecated, Some(false));
    }

    #[test]
    fn invalid_json_returns_error() {
        let result = parse_feature_metadata("not json at all");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FeatureError::InvalidMetadata { feature_id, .. } if feature_id == "<unknown>"),
            "expected InvalidMetadata with unknown id, got: {err:?}"
        );
    }

    #[test]
    fn missing_id_returns_error() {
        let json = r#"{"version": "1.0.0"}"#;
        let result = parse_feature_metadata(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FeatureError::InvalidMetadata { feature_id, reason } if feature_id == "<unknown>" && reason.contains("id")),
            "expected InvalidMetadata mentioning 'id', got: {err:?}"
        );
    }

    #[test]
    fn missing_version_returns_error() {
        let json = r#"{"id": "test-feature"}"#;
        let result = parse_feature_metadata(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FeatureError::InvalidMetadata { feature_id, reason } if feature_id == "test-feature" && reason.contains("version")),
            "expected InvalidMetadata mentioning 'version', got: {err:?}"
        );
    }

    #[test]
    fn string_option_type() {
        let json = r#"{
            "id": "test",
            "version": "1.0.0",
            "options": {
                "flavor": {
                    "type": "string",
                    "default": "vanilla",
                    "description": "The flavor to use"
                }
            }
        }"#;
        let meta = parse_feature_metadata(json).unwrap();
        let opt = &meta.options["flavor"];
        assert_eq!(opt.option_type, OptionType::String);
        assert_eq!(opt.default, serde_json::json!("vanilla"));
        assert_eq!(opt.description.as_deref(), Some("The flavor to use"));
        assert!(opt.enum_values.is_none());
    }

    #[test]
    fn boolean_option_type() {
        let json = r#"{
            "id": "test",
            "version": "1.0.0",
            "options": {
                "enabled": {
                    "type": "boolean",
                    "default": false
                }
            }
        }"#;
        let meta = parse_feature_metadata(json).unwrap();
        let opt = &meta.options["enabled"];
        assert_eq!(opt.option_type, OptionType::Boolean);
        assert_eq!(opt.default, serde_json::json!(false));
        assert!(opt.description.is_none());
    }

    #[test]
    fn enum_option() {
        let json = r#"{
            "id": "test",
            "version": "1.0.0",
            "options": {
                "color": {
                    "type": "string",
                    "default": "red",
                    "enum": ["red", "green", "blue"]
                }
            }
        }"#;
        let meta = parse_feature_metadata(json).unwrap();
        let opt = &meta.options["color"];
        assert_eq!(opt.option_type, OptionType::String);
        assert_eq!(
            opt.enum_values.as_deref(),
            Some(&["red", "green", "blue"].map(String::from)[..])
        );
    }

    #[test]
    fn unknown_option_type_returns_error() {
        let json = r#"{
            "id": "bad-feature",
            "version": "1.0.0",
            "options": {
                "count": {
                    "type": "integer",
                    "default": 42
                }
            }
        }"#;
        let result = parse_feature_metadata(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FeatureError::InvalidMetadata { feature_id, reason }
                if feature_id == "bad-feature" && reason.contains("integer")),
            "expected InvalidMetadata about unknown type, got: {err:?}"
        );
    }

    #[test]
    fn option_with_no_default_uses_json_null() {
        let json = r#"{
            "id": "test",
            "version": "1.0.0",
            "options": {
                "name": {
                    "type": "string"
                }
            }
        }"#;
        let meta = parse_feature_metadata(json).unwrap();
        let opt = &meta.options["name"];
        assert!(opt.default.is_null());
    }

    #[test]
    fn extra_unknown_fields_are_ignored() {
        let json = r#"{
            "id": "test",
            "version": "1.0.0",
            "someFutureField": "should not cause failure",
            "anotherOne": 42
        }"#;
        let meta = parse_feature_metadata(json).unwrap();
        assert_eq!(meta.id, "test");
    }
}
