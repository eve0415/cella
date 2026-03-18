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
