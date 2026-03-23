use indexmap::IndexMap;

use super::{AdditionalProperties, PrimitiveType, SchemaNode, SchemaType};
use crate::CellaCodegenError;

/// The result of parsing a root JSON Schema document.
#[derive(Debug)]
pub struct ParsedSchema {
    pub definitions: IndexMap<String, SchemaNode>,
    pub root: SchemaNode,
}

/// Parse a root JSON Schema document from a `serde_json::Value`.
pub fn parse_root_schema(value: &serde_json::Value) -> Result<ParsedSchema, CellaCodegenError> {
    let obj = value
        .as_object()
        .ok_or_else(|| CellaCodegenError::Schema("root schema must be an object".into()))?;

    let definitions = if let Some(defs) = obj.get("definitions") {
        parse_definitions(defs)?
    } else {
        IndexMap::new()
    };

    let root = parse_schema_node(value)?;

    Ok(ParsedSchema { definitions, root })
}

fn parse_definitions(
    value: &serde_json::Value,
) -> Result<IndexMap<String, SchemaNode>, CellaCodegenError> {
    let obj = value
        .as_object()
        .ok_or_else(|| CellaCodegenError::Schema("definitions must be an object".into()))?;

    let mut defs = IndexMap::new();
    for (name, def_value) in obj {
        defs.insert(name.clone(), parse_schema_node(def_value)?);
    }
    Ok(defs)
}

/// Parse a single JSON Schema node.
pub fn parse_schema_node(value: &serde_json::Value) -> Result<SchemaNode, CellaCodegenError> {
    let Some(obj) = value.as_object() else {
        // Boolean schemas: true = anything validates, false = nothing validates
        if value.is_boolean() {
            return Ok(SchemaNode::default());
        }
        return Err(CellaCodegenError::Schema(
            "schema node must be object or boolean".into(),
        ));
    };

    let mut node = SchemaNode {
        description: get_str(obj, "description"),
        deprecated: obj
            .get("deprecated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        r#ref: get_str(obj, "$ref"),
        ..SchemaNode::default()
    };

    // Type
    if let Some(type_val) = obj.get("type") {
        node.schema_type = Some(parse_type(type_val)?);
    }

    // Properties
    if let Some(props) = obj.get("properties").and_then(serde_json::Value::as_object) {
        for (name, prop_val) in props {
            node.properties
                .insert(name.clone(), parse_schema_node(prop_val)?);
        }
    }

    // Required
    if let Some(arr) = obj.get("required").and_then(serde_json::Value::as_array) {
        for item in arr {
            if let Some(s) = item.as_str() {
                node.required.push(s.to_string());
            }
        }
    }

    // Additional properties
    if let Some(ap) = obj.get("additionalProperties") {
        node.additional_properties = Some(parse_additional_properties(ap)?);
    }

    // Pattern properties
    if let Some(pp) = obj
        .get("patternProperties")
        .and_then(serde_json::Value::as_object)
    {
        for (pattern, schema) in pp {
            node.pattern_properties
                .insert(pattern.clone(), parse_schema_node(schema)?);
        }
    }

    // Items
    if let Some(items) = obj.get("items") {
        node.items = Some(Box::new(parse_schema_node(items)?));
    }

    // Composition keywords
    node.one_of = parse_schema_array(obj, "oneOf")?;
    node.all_of = parse_schema_array(obj, "allOf")?;
    node.any_of = parse_schema_array(obj, "anyOf")?;

    // Enum
    if let Some(arr) = obj.get("enum").and_then(serde_json::Value::as_array) {
        node.enum_values.clone_from(arr);
    }

    // Numeric constraints
    node.minimum = obj.get("minimum").and_then(serde_json::Value::as_f64);
    node.maximum = obj.get("maximum").and_then(serde_json::Value::as_f64);

    // Unevaluated properties
    node.unevaluated_properties = obj
        .get("unevaluatedProperties")
        .and_then(serde_json::Value::as_bool);

    // Nested definitions
    if let Some(defs) = obj.get("definitions")
        && let Some(defs_obj) = defs.as_object()
    {
        for (name, def_val) in defs_obj {
            node.definitions
                .insert(name.clone(), parse_schema_node(def_val)?);
        }
    }

    Ok(node)
}

fn parse_type(value: &serde_json::Value) -> Result<SchemaType, CellaCodegenError> {
    match value {
        serde_json::Value::String(s) => Ok(SchemaType::Single(parse_primitive_type(s)?)),
        serde_json::Value::Array(arr) => {
            let types = arr
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(parse_primitive_type)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SchemaType::Multi(types))
        }
        _ => Err(CellaCodegenError::Schema(
            "type must be string or array".into(),
        )),
    }
}

fn parse_primitive_type(s: &str) -> Result<PrimitiveType, CellaCodegenError> {
    match s {
        "string" => Ok(PrimitiveType::String),
        "integer" => Ok(PrimitiveType::Integer),
        "number" => Ok(PrimitiveType::Number),
        "boolean" => Ok(PrimitiveType::Boolean),
        "object" => Ok(PrimitiveType::Object),
        "array" => Ok(PrimitiveType::Array),
        "null" => Ok(PrimitiveType::Null),
        _ => Err(CellaCodegenError::Schema(format!("unknown type: {s}"))),
    }
}

fn parse_additional_properties(
    value: &serde_json::Value,
) -> Result<AdditionalProperties, CellaCodegenError> {
    match value {
        serde_json::Value::Bool(b) => Ok(AdditionalProperties::Bool(*b)),
        _ => Ok(AdditionalProperties::Schema(Box::new(parse_schema_node(
            value,
        )?))),
    }
}

fn parse_schema_array(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<SchemaNode>, CellaCodegenError> {
    obj.get(key)
        .and_then(serde_json::Value::as_array)
        .map_or_else(
            || Ok(Vec::new()),
            |arr| arr.iter().map(parse_schema_node).collect(),
        )
}

fn get_str(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(json: &serde_json::Value) -> ParsedSchema {
        parse_root_schema(json).unwrap()
    }

    #[test]
    fn empty_schema() {
        let result = parse(&json!({}));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn single_string_property() {
        let result = parse(&json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn nested_objects() {
        let result = parse(&json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "object",
                    "properties": {
                        "street": { "type": "string" },
                        "city": { "type": "string" }
                    }
                }
            }
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn array_with_items() {
        let result = parse(&json!({
            "type": "array",
            "items": { "type": "string" }
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn enum_values() {
        let result = parse(&json!({
            "type": "string",
            "enum": ["a", "b", "c"]
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn one_of() {
        let result = parse(&json!({
            "oneOf": [
                { "type": "string" },
                { "type": "integer" }
            ]
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn all_of() {
        let result = parse(&json!({
            "allOf": [
                { "type": "object", "properties": { "a": { "type": "string" } } },
                { "type": "object", "properties": { "b": { "type": "integer" } } }
            ]
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn any_of() {
        let result = parse(&json!({
            "anyOf": [
                { "type": "string" },
                { "type": "boolean" }
            ]
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn ref_node() {
        let result = parse(&json!({
            "$ref": "#/definitions/Foo"
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn definitions() {
        let result = parse(&json!({
            "type": "object",
            "definitions": {
                "Foo": { "type": "string" },
                "Bar": { "type": "integer" }
            }
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn multi_type() {
        let result = parse(&json!({
            "type": ["string", "null"]
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn additional_properties_bool() {
        let result = parse(&json!({
            "type": "object",
            "additionalProperties": false
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn additional_properties_schema() {
        let result = parse(&json!({
            "type": "object",
            "additionalProperties": { "type": "string" }
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn boolean_schema_true() {
        // Boolean schema `true` parsed at the root level should fail
        // because root must be an object. Test parse_schema_node directly.
        let result = parse_schema_node(&json!(true)).unwrap();
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn deprecated_field() {
        let result = parse(&json!({
            "type": "string",
            "deprecated": true,
            "deprecationMessage": "use X instead"
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn pattern_properties() {
        let result = parse(&json!({
            "type": "object",
            "patternProperties": {
                "^x-": { "type": "string" }
            }
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn numeric_constraints() {
        let result = parse(&json!({
            "type": "integer",
            "minimum": 0,
            "maximum": 100
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn required_fields() {
        let result = parse(&json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" }
            },
            "required": ["a"]
        }));
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn error_non_object_root() {
        let result = parse_root_schema(&json!(42));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("root schema must be an object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_unknown_type() {
        let result = parse_root_schema(&json!({"type": "foobar"}));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown type: foobar"),
            "unexpected error: {err}"
        );
    }
}
