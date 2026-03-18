use indexmap::IndexMap;

use super::{AdditionalProperties, SchemaNode};
use crate::CellaCodegenError;

/// Resolve all `$ref` nodes in a schema by looking up definitions.
#[allow(dead_code)]
pub fn resolve_refs(
    node: &SchemaNode,
    definitions: &IndexMap<String, SchemaNode>,
) -> Result<SchemaNode, CellaCodegenError> {
    resolve_node(node, definitions)
}

#[allow(dead_code)]
fn resolve_node(
    node: &SchemaNode,
    definitions: &IndexMap<String, SchemaNode>,
) -> Result<SchemaNode, CellaCodegenError> {
    if let Some(ref_path) = &node.r#ref {
        let def_name = ref_path
            .strip_prefix("#/definitions/")
            .ok_or_else(|| CellaCodegenError::Schema(format!("unsupported $ref: {ref_path}")))?;

        let target = definitions
            .get(def_name)
            .ok_or_else(|| CellaCodegenError::Schema(format!("unresolved $ref: {ref_path}")))?;

        // If the node has additional constraints beyond $ref, merge them
        if has_own_constraints(node) {
            return merge_with_ref(node, target, definitions);
        }

        return resolve_node(target, definitions);
    }

    let mut resolved = node.clone();

    // Resolve properties
    let mut resolved_props = IndexMap::new();
    for (name, prop) in &node.properties {
        resolved_props.insert(name.clone(), resolve_node(prop, definitions)?);
    }
    resolved.properties = resolved_props;

    // Resolve items
    if let Some(items) = &node.items {
        resolved.items = Some(Box::new(resolve_node(items, definitions)?));
    }

    // Resolve composition keywords
    resolved.one_of = resolve_vec(&node.one_of, definitions)?;
    resolved.all_of = resolve_vec(&node.all_of, definitions)?;
    resolved.any_of = resolve_vec(&node.any_of, definitions)?;

    // Resolve pattern properties
    let mut resolved_pp = IndexMap::new();
    for (pattern, schema) in &node.pattern_properties {
        resolved_pp.insert(pattern.clone(), resolve_node(schema, definitions)?);
    }
    resolved.pattern_properties = resolved_pp;

    // Resolve additional properties
    if let Some(AdditionalProperties::Schema(schema)) = &node.additional_properties {
        resolved.additional_properties = Some(AdditionalProperties::Schema(Box::new(
            resolve_node(schema, definitions)?,
        )));
    }

    Ok(resolved)
}

#[allow(dead_code)]
fn resolve_vec(
    nodes: &[SchemaNode],
    definitions: &IndexMap<String, SchemaNode>,
) -> Result<Vec<SchemaNode>, CellaCodegenError> {
    nodes.iter().map(|n| resolve_node(n, definitions)).collect()
}

#[allow(dead_code)]
fn has_own_constraints(node: &SchemaNode) -> bool {
    node.schema_type.is_some()
        || !node.properties.is_empty()
        || !node.one_of.is_empty()
        || !node.all_of.is_empty()
        || !node.any_of.is_empty()
        || !node.enum_values.is_empty()
        || node.additional_properties.is_some()
        || node.unevaluated_properties.is_some()
}

#[allow(dead_code)]
fn merge_with_ref(
    node: &SchemaNode,
    target: &SchemaNode,
    definitions: &IndexMap<String, SchemaNode>,
) -> Result<SchemaNode, CellaCodegenError> {
    let mut merged = resolve_node(target, definitions)?;

    if node.schema_type.is_some() {
        merged.schema_type.clone_from(&node.schema_type);
    }
    for (k, v) in &node.properties {
        merged
            .properties
            .insert(k.clone(), resolve_node(v, definitions)?);
    }
    if !node.required.is_empty() {
        merged.required.extend(node.required.iter().cloned());
    }
    if node.additional_properties.is_some() {
        merged
            .additional_properties
            .clone_from(&node.additional_properties);
    }
    if node.unevaluated_properties.is_some() {
        merged.unevaluated_properties = node.unevaluated_properties;
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AdditionalProperties, PrimitiveType, SchemaNode, SchemaType};
    use indexmap::IndexMap;

    fn make_definitions() -> IndexMap<String, SchemaNode> {
        let mut defs = IndexMap::new();
        defs.insert(
            "Foo".to_string(),
            SchemaNode {
                schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                description: Some("A foo string".to_string()),
                ..SchemaNode::default()
            },
        );
        defs.insert("Bar".to_string(), {
            let mut bar = SchemaNode {
                schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
                ..SchemaNode::default()
            };
            bar.properties.insert(
                "name".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                    ..SchemaNode::default()
                },
            );
            bar
        });
        defs
    }

    #[test]
    fn simple_ref_resolution() {
        let defs = make_definitions();
        let node = SchemaNode {
            r#ref: Some("#/definitions/Foo".to_string()),
            ..SchemaNode::default()
        };
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn nested_ref_in_property() {
        let defs = make_definitions();
        let mut node = SchemaNode {
            schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
            ..SchemaNode::default()
        };
        node.properties.insert(
            "my_field".to_string(),
            SchemaNode {
                r#ref: Some("#/definitions/Foo".to_string()),
                ..SchemaNode::default()
            },
        );
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn ref_with_extra_constraints() {
        let defs = make_definitions();
        let mut node = SchemaNode {
            r#ref: Some("#/definitions/Bar".to_string()),
            schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
            ..SchemaNode::default()
        };
        node.properties.insert(
            "extra".to_string(),
            SchemaNode {
                schema_type: Some(SchemaType::Single(PrimitiveType::Integer)),
                ..SchemaNode::default()
            },
        );
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn missing_ref_error() {
        let defs = make_definitions();
        let node = SchemaNode {
            r#ref: Some("#/definitions/DoesNotExist".to_string()),
            ..SchemaNode::default()
        };
        let err = resolve_refs(&node, &defs).unwrap_err();
        assert!(
            err.to_string().contains("unresolved"),
            "expected 'unresolved' in error, got: {err}"
        );
    }

    #[test]
    fn unsupported_ref_format() {
        let defs = make_definitions();
        let node = SchemaNode {
            r#ref: Some("other/path/Foo".to_string()),
            ..SchemaNode::default()
        };
        let err = resolve_refs(&node, &defs).unwrap_err();
        assert!(
            err.to_string().contains("unsupported"),
            "expected 'unsupported' in error, got: {err}"
        );
    }

    #[test]
    fn no_ref_passthrough() {
        let defs = make_definitions();
        let node = SchemaNode {
            schema_type: Some(SchemaType::Single(PrimitiveType::Boolean)),
            description: Some("a bool".to_string()),
            ..SchemaNode::default()
        };
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn resolve_through_items() {
        let defs = make_definitions();
        let node = SchemaNode {
            schema_type: Some(SchemaType::Single(PrimitiveType::Array)),
            items: Some(Box::new(SchemaNode {
                r#ref: Some("#/definitions/Foo".to_string()),
                ..SchemaNode::default()
            })),
            ..SchemaNode::default()
        };
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn resolve_through_one_of() {
        let defs = make_definitions();
        let node = SchemaNode {
            one_of: vec![
                SchemaNode {
                    r#ref: Some("#/definitions/Foo".to_string()),
                    ..SchemaNode::default()
                },
                SchemaNode {
                    r#ref: Some("#/definitions/Bar".to_string()),
                    ..SchemaNode::default()
                },
            ],
            ..SchemaNode::default()
        };
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn resolve_additional_properties_schema() {
        let defs = make_definitions();
        let node = SchemaNode {
            schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
            additional_properties: Some(AdditionalProperties::Schema(Box::new(SchemaNode {
                r#ref: Some("#/definitions/Foo".to_string()),
                ..SchemaNode::default()
            }))),
            ..SchemaNode::default()
        };
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }

    #[test]
    fn resolve_pattern_properties() {
        let defs = make_definitions();
        let mut node = SchemaNode {
            schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
            ..SchemaNode::default()
        };
        node.pattern_properties.insert(
            "^x-".to_string(),
            SchemaNode {
                r#ref: Some("#/definitions/Foo".to_string()),
                ..SchemaNode::default()
            },
        );
        let resolved = resolve_refs(&node, &defs).unwrap();
        insta::assert_debug_snapshot!(resolved);
    }
}
