mod emit;
mod error;
mod ir;
mod schema;

pub use error::CellaCodegenError;

/// Configuration for the code generator.
pub struct CodegenConfig {
    /// The name for the root generated type (e.g., `"DevContainer"`).
    pub root_type_name: String,
    /// Whether to emit `#[doc = "..."]` attributes from schema descriptions.
    pub emit_docs: bool,
    /// Whether to emit `#[deprecated]` attributes on deprecated schema fields.
    pub emit_deprecated: bool,
}

/// Generate Rust types and validators from a JSON Schema string.
///
/// Returns a formatted Rust source string suitable for `include!()`.
///
/// # Errors
///
/// Returns `CellaCodegenError` if parsing, lowering, emission, or formatting fails.
pub fn generate(schema_json: &str, config: &CodegenConfig) -> Result<String, CellaCodegenError> {
    let value: serde_json::Value = serde_json::from_str(schema_json)?;
    let parsed = schema::parse::parse_root_schema(&value)?;
    let ir_types = ir::lower::lower(&parsed.definitions, &parsed.root, &config.root_type_name);
    let tokens = emit::emit_all(&ir_types, config);
    emit::format::format_tokens(&tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> CodegenConfig {
        CodegenConfig {
            root_type_name: "Root".to_string(),
            emit_docs: true,
            emit_deprecated: true,
        }
    }

    #[test]
    fn minimal_schema() {
        let schema = r#"{"type": "object", "properties": {"name": {"type": "string"}}}"#;
        let result = generate(schema, &default_config()).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn schema_with_definitions_and_ref() {
        let schema = r##"{
            "type": "object",
            "definitions": {
                "Address": {
                    "type": "object",
                    "properties": {
                        "street": {"type": "string"},
                        "city": {"type": "string"}
                    }
                }
            },
            "properties": {
                "home_address": {"$ref": "#/definitions/Address"}
            }
        }"##;
        let result = generate(schema, &default_config()).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn enum_only_schema() {
        let schema = r#"{"type": "string", "enum": ["a", "b", "c"]}"#;
        let result = generate(schema, &default_config()).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn all_of_schema() {
        let schema = r##"{
            "definitions": {
                "Base": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"}
                    }
                },
                "Extra": {
                    "type": "object",
                    "properties": {
                        "label": {"type": "string"}
                    }
                }
            },
            "allOf": [
                {"$ref": "#/definitions/Base"},
                {"$ref": "#/definitions/Extra"}
            ]
        }"##;
        let result = generate(schema, &default_config()).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn emit_docs_false() {
        let schema = r#"{
            "type": "object",
            "description": "A person record",
            "properties": {
                "name": {"type": "string", "description": "The person's name"}
            }
        }"#;
        let config = CodegenConfig {
            emit_docs: false,
            ..default_config()
        };
        let result = generate(schema, &config).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn emit_docs_true() {
        let schema = r#"{
            "type": "object",
            "description": "A person record",
            "properties": {
                "name": {"type": "string", "description": "The person's name"}
            }
        }"#;
        let config = CodegenConfig {
            emit_docs: true,
            ..default_config()
        };
        let result = generate(schema, &config).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn error_invalid_json() {
        let result = generate("not valid json", &default_config());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.to_lowercase().contains("expected")
                || err.to_lowercase().contains("json")
                || err.to_lowercase().contains("parse"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn error_non_object_root() {
        let result = generate("42", &default_config());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.to_lowercase().contains("object")
                || err.to_lowercase().contains("root")
                || err.to_lowercase().contains("schema"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn full_devcontainer_schema() {
        let schema_json = include_str!("../../cella-config/schemas/devContainer.base.schema.json");
        let config = CodegenConfig {
            root_type_name: "DevContainer".to_string(),
            emit_docs: true,
            emit_deprecated: true,
        };
        let result = generate(schema_json, &config).unwrap();
        insta::assert_snapshot!(result);
    }
}
