use indexmap::IndexMap;

use super::naming::{ref_to_def_name, to_rust_field_name, to_rust_type_name, to_variant_name};
use super::{EnumRepr, IrAlias, IrEnum, IrField, IrStruct, IrType, IrTypeRef, IrVariant};
use crate::schema::{AdditionalProperties, PrimitiveType, SchemaNode, SchemaType};

/// Lower a parsed schema into IR types.
pub fn lower(
    definitions: &IndexMap<String, SchemaNode>,
    root: &SchemaNode,
    root_type_name: &str,
) -> Vec<IrType> {
    let mut lowerer = Lowerer {
        types: Vec::new(),
        definitions: definitions.clone(),
    };

    // Lower each definition as a named type
    for (name, node) in definitions {
        let type_name = to_rust_type_name(name);
        lowerer.lower_definition(&type_name, node);
    }

    // Lower the root schema
    lowerer.lower_root(root_type_name, root);

    lowerer.types
}

struct Lowerer {
    types: Vec<IrType>,
    #[allow(dead_code)]
    definitions: IndexMap<String, SchemaNode>,
}

impl Lowerer {
    fn lower_definition(&mut self, name: &str, node: &SchemaNode) {
        // Skip if this definition would result in an empty / trivial type
        // that we can represent inline
        let ir = if !node.one_of.is_empty() {
            self.lower_one_of(name, node)
        } else if !node.all_of.is_empty() {
            self.lower_all_of(name, &node.all_of, node)
        } else if !node.any_of.is_empty() {
            self.lower_any_of(name, node)
        } else if !node.enum_values.is_empty() && node.properties.is_empty() {
            self.lower_string_enum(name, node)
        } else if !node.properties.is_empty() || self.is_object_type(node) {
            self.lower_struct(name, node)
        } else {
            IrType::Alias(IrAlias {
                name: name.to_string(),
                doc: node.description.clone(),
                ty: IrTypeRef::Value,
            })
        };
        self.types.push(ir);
    }

    fn lower_root(&mut self, name: &str, node: &SchemaNode) {
        if !node.one_of.is_empty() {
            let ir = self.lower_one_of(name, node);
            self.types.push(ir);
        } else if !node.all_of.is_empty() {
            let ir = self.lower_all_of(name, &node.all_of, node);
            self.types.push(ir);
        } else if !node.properties.is_empty() {
            let ir = self.lower_struct(name, node);
            self.types.push(ir);
        } else {
            self.types.push(IrType::Alias(IrAlias {
                name: name.to_string(),
                doc: node.description.clone(),
                ty: IrTypeRef::Value,
            }));
        }
    }

    // ── oneOf ────────────────────────────────────────────────────────────

    fn lower_one_of(&mut self, name: &str, node: &SchemaNode) -> IrType {
        let variants: Vec<IrVariant> = node
            .one_of
            .iter()
            .enumerate()
            .map(|(i, branch)| self.lower_one_of_variant(name, i, branch))
            .collect();

        IrType::Enum(IrEnum {
            name: name.to_string(),
            doc: node.description.clone(),
            variants,
            repr: EnumRepr::TypedVariants,
        })
    }

    fn lower_one_of_variant(
        &mut self,
        parent: &str,
        index: usize,
        branch: &SchemaNode,
    ) -> IrVariant {
        // If it's a $ref, use the definition name
        if let Some(ref_path) = &branch.r#ref
            && let Some(def_name) = ref_to_def_name(ref_path)
        {
            let type_name = to_rust_type_name(def_name);
            return IrVariant {
                name: type_name.clone(),
                doc: branch.description.clone(),
                json_value: None,
                ty: Some(IrTypeRef::Named(type_name)),
            };
        }

        // Use lower_type_ref which handles simple types (int, string, etc.)
        // as well as complex inline schemas
        let variant_name = format!("{parent}Variant{index}");
        let ty = self.lower_type_ref(branch, &variant_name);

        IrVariant {
            name: variant_name.clone(),
            doc: branch.description.clone(),
            json_value: None,
            ty: Some(ty),
        }
    }

    // ── allOf ────────────────────────────────────────────────────────────

    fn lower_all_of(
        &mut self,
        name: &str,
        members: &[SchemaNode],
        parent_node: &SchemaNode,
    ) -> IrType {
        let mut fields = Vec::new();

        for (i, member) in members.iter().enumerate() {
            let (field_name, field_ty) = if let Some(ref_path) = &member.r#ref {
                // $ref → compose as named field
                ref_to_def_name(ref_path).map_or_else(
                    || (format!("part_{i}"), IrTypeRef::Value),
                    |def_name| {
                        let type_name = to_rust_type_name(def_name);
                        (to_rust_field_name(def_name), IrTypeRef::Named(type_name))
                    },
                )
            } else if !member.one_of.is_empty()
                || !member.all_of.is_empty()
                || !member.any_of.is_empty()
            {
                // Complex member → generate auxiliary type
                let part_name = format!("{name}Part{i}");
                let ty = self.lower_inline_to_named(&part_name, member);
                (format!("part_{i}"), ty)
            } else if !member.properties.is_empty() || self.is_object_type(member) {
                // Inline object → generate auxiliary struct
                let part_name = format!("{name}Part{i}");
                let st = self.lower_struct(&part_name, member);
                self.types.push(st);
                (format!("part_{i}"), IrTypeRef::Named(part_name))
            } else {
                (format!("part_{i}"), IrTypeRef::Value)
            };

            fields.push(IrField {
                name: field_name,
                json_name: String::new(), // allOf fields don't map to JSON properties
                doc: None,
                ty: field_ty,
                required: true,
                deprecated: false,
            });
        }

        let deny_unknown =
            parent_node.unevaluated_properties == Some(false) || parent_node.denies_additional();

        IrType::Struct(IrStruct {
            name: name.to_string(),
            doc: parent_node.description.clone(),
            fields,
            deny_unknown_fields: deny_unknown,
            is_all_of: true,
        })
    }

    // ── anyOf ────────────────────────────────────────────────────────────

    fn lower_any_of(&mut self, name: &str, node: &SchemaNode) -> IrType {
        let variants: Vec<IrVariant> = node
            .any_of
            .iter()
            .enumerate()
            .map(|(i, branch)| {
                if let Some(ref_path) = &branch.r#ref
                    && let Some(def_name) = ref_to_def_name(ref_path)
                {
                    let type_name = to_rust_type_name(def_name);
                    return IrVariant {
                        name: type_name.clone(),
                        doc: branch.description.clone(),
                        json_value: None,
                        ty: Some(IrTypeRef::Named(type_name)),
                    };
                }
                let inner_ty = self.lower_type_ref(branch, &format!("{name}Variant{i}"));
                IrVariant {
                    name: format!("Variant{i}"),
                    doc: branch.description.clone(),
                    json_value: None,
                    ty: Some(inner_ty),
                }
            })
            .collect();

        IrType::Enum(IrEnum {
            name: name.to_string(),
            doc: node.description.clone(),
            variants,
            repr: EnumRepr::TypedVariants,
        })
    }

    // ── Struct ───────────────────────────────────────────────────────────

    fn lower_struct(&mut self, name: &str, node: &SchemaNode) -> IrType {
        let fields: Vec<IrField> = node
            .properties
            .iter()
            .map(|(prop_name, prop_schema)| {
                let context = format!("{name}{}", to_rust_type_name(prop_name));
                let ty = self.lower_type_ref(prop_schema, &context);
                let required = node.required.contains(prop_name);

                IrField {
                    name: to_rust_field_name(prop_name),
                    json_name: prop_name.clone(),
                    doc: prop_schema.description.clone(),
                    ty,
                    required,
                    deprecated: prop_schema.deprecated,
                }
            })
            .collect();

        let deny_unknown = node.denies_additional() || node.unevaluated_properties == Some(false);

        IrType::Struct(IrStruct {
            name: name.to_string(),
            doc: node.description.clone(),
            fields,
            deny_unknown_fields: deny_unknown,
            is_all_of: false,
        })
    }

    // ── String enum ─────────────────────────────────────────────────────

    #[allow(clippy::unused_self)]
    fn lower_string_enum(&self, name: &str, node: &SchemaNode) -> IrType {
        let has_bool = node.enum_values.iter().any(serde_json::Value::is_boolean);
        let has_string = node.enum_values.iter().any(serde_json::Value::is_string);

        let repr = if has_bool && has_string {
            EnumRepr::BoolMixed
        } else {
            EnumRepr::StringEnum
        };

        let variants = node
            .enum_values
            .iter()
            .filter_map(|v| {
                let variant_name = match v {
                    serde_json::Value::String(s) => to_variant_name(s),
                    serde_json::Value::Bool(b) => {
                        if *b {
                            "True".to_string()
                        } else {
                            "False".to_string()
                        }
                    }
                    _ => return None,
                };
                Some(IrVariant {
                    name: variant_name,
                    doc: None,
                    json_value: Some(v.clone()),
                    ty: None,
                })
            })
            .collect();

        IrType::Enum(IrEnum {
            name: name.to_string(),
            doc: node.description.clone(),
            variants,
            repr,
        })
    }

    // ── Type reference lowering ─────────────────────────────────────────

    fn lower_type_ref(&mut self, node: &SchemaNode, context_name: &str) -> IrTypeRef {
        // $ref → Named
        if let Some(ref_path) = &node.r#ref
            && let Some(def_name) = ref_to_def_name(ref_path)
        {
            return IrTypeRef::Named(to_rust_type_name(def_name));
        }

        // Multi-type: type: ["string", "null"]
        if let Some(SchemaType::Multi(types)) = &node.schema_type {
            return self.lower_multi_type(types, node, context_name);
        }

        // Single type
        if let Some(SchemaType::Single(pt)) = &node.schema_type {
            return self.lower_single_type(pt, node, context_name);
        }

        // oneOf inline
        if !node.one_of.is_empty() {
            return self.lower_inline_to_named(context_name, node);
        }

        // allOf inline
        if !node.all_of.is_empty() {
            let ir = self.lower_all_of(context_name, &node.all_of, node);
            self.types.push(ir);
            return IrTypeRef::Named(context_name.to_string());
        }

        // anyOf inline
        if !node.any_of.is_empty() {
            return self.lower_inline_to_named(context_name, node);
        }

        // Enum values without a type
        if !node.enum_values.is_empty() {
            let ir = self.lower_string_enum(context_name, node);
            self.types.push(ir);
            return IrTypeRef::Named(context_name.to_string());
        }

        // Properties without explicit type → object
        if !node.properties.is_empty() {
            let ir = self.lower_struct(context_name, node);
            self.types.push(ir);
            return IrTypeRef::Named(context_name.to_string());
        }

        IrTypeRef::Value
    }

    fn lower_single_type(
        &mut self,
        pt: &PrimitiveType,
        node: &SchemaNode,
        context_name: &str,
    ) -> IrTypeRef {
        match pt {
            PrimitiveType::String => {
                if node.enum_values.is_empty() {
                    IrTypeRef::String
                } else {
                    let ir = self.lower_string_enum(context_name, node);
                    self.types.push(ir);
                    IrTypeRef::Named(context_name.to_string())
                }
            }
            PrimitiveType::Integer => IrTypeRef::I64,
            PrimitiveType::Number => IrTypeRef::F64,
            PrimitiveType::Boolean => IrTypeRef::Bool,
            PrimitiveType::Object => self.lower_object_type(node, context_name),
            PrimitiveType::Array => node.items.as_ref().map_or_else(
                || IrTypeRef::Vec(Box::new(IrTypeRef::Value)),
                |items| {
                    let item_name = format!("{context_name}Item");
                    let item_ty = self.lower_type_ref(items, &item_name);
                    IrTypeRef::Vec(Box::new(item_ty))
                },
            ),
            PrimitiveType::Null => IrTypeRef::Value,
        }
    }

    fn lower_object_type(&mut self, node: &SchemaNode, context_name: &str) -> IrTypeRef {
        if !node.properties.is_empty() {
            let ir = self.lower_struct(context_name, node);
            self.types.push(ir);
            IrTypeRef::Named(context_name.to_string())
        } else if let Some(AdditionalProperties::Schema(ap_schema)) = &node.additional_properties {
            let value_name = format!("{context_name}Value");
            let value_ty = self.lower_type_ref(ap_schema, &value_name);
            IrTypeRef::Map(Box::new(IrTypeRef::String), Box::new(value_ty))
        } else {
            IrTypeRef::Map(Box::new(IrTypeRef::String), Box::new(IrTypeRef::Value))
        }
    }

    fn lower_multi_type(
        &mut self,
        types: &[PrimitiveType],
        node: &SchemaNode,
        context_name: &str,
    ) -> IrTypeRef {
        let non_null: Vec<_> = types
            .iter()
            .filter(|t| **t != PrimitiveType::Null)
            .collect();
        let has_null = types.contains(&PrimitiveType::Null);

        if non_null.len() == 1 {
            let inner = self.lower_single_type(non_null[0], node, context_name);
            if has_null {
                IrTypeRef::Option(Box::new(inner))
            } else {
                inner
            }
        } else {
            // Multiple non-null types → generate multi-type enum
            let variants: Vec<IrVariant> = non_null
                .iter()
                .map(|pt| {
                    let (vname, ty) = match pt {
                        PrimitiveType::String => ("String".to_string(), IrTypeRef::String),
                        PrimitiveType::Integer => ("Integer".to_string(), IrTypeRef::I64),
                        PrimitiveType::Number => ("Number".to_string(), IrTypeRef::F64),
                        PrimitiveType::Boolean => ("Boolean".to_string(), IrTypeRef::Bool),
                        PrimitiveType::Array => {
                            let item_ty = node.items.as_ref().map_or(IrTypeRef::Value, |items| {
                                let item_name = format!("{context_name}Item");
                                self.lower_type_ref(items, &item_name)
                            });
                            ("Array".to_string(), IrTypeRef::Vec(Box::new(item_ty)))
                        }
                        PrimitiveType::Object => {
                            let obj_ty =
                                self.lower_object_type(node, &format!("{context_name}Object"));
                            ("Object".to_string(), obj_ty)
                        }
                        PrimitiveType::Null => unreachable!(),
                    };
                    IrVariant {
                        name: vname,
                        doc: None,
                        json_value: None,
                        ty: Some(ty),
                    }
                })
                .collect();

            let ir = IrType::Enum(IrEnum {
                name: context_name.to_string(),
                doc: node.description.clone(),
                variants,
                repr: EnumRepr::MultiType,
            });
            self.types.push(ir);

            let named = IrTypeRef::Named(context_name.to_string());
            if has_null {
                IrTypeRef::Option(Box::new(named))
            } else {
                named
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Lower an inline schema to a named type and return a `Named` reference.
    fn lower_inline_to_named(&mut self, name: &str, node: &SchemaNode) -> IrTypeRef {
        if !node.one_of.is_empty() {
            let ir = self.lower_one_of(name, node);
            self.types.push(ir);
        } else if !node.all_of.is_empty() {
            let ir = self.lower_all_of(name, &node.all_of, node);
            self.types.push(ir);
        } else if !node.any_of.is_empty() {
            let ir = self.lower_any_of(name, node);
            self.types.push(ir);
        } else if !node.enum_values.is_empty() && node.properties.is_empty() {
            let ir = self.lower_string_enum(name, node);
            self.types.push(ir);
        } else if !node.properties.is_empty() || self.is_object_type(node) {
            let ir = self.lower_struct(name, node);
            self.types.push(ir);
        } else if node.schema_type.is_some() {
            // Simple typed node — return the type directly instead of wrapping
            return self.lower_type_ref(node, name);
        } else {
            self.types.push(IrType::Alias(IrAlias {
                name: name.to_string(),
                doc: node.description.clone(),
                ty: IrTypeRef::Value,
            }));
        }
        IrTypeRef::Named(name.to_string())
    }

    #[allow(clippy::unused_self)]
    const fn is_object_type(&self, node: &SchemaNode) -> bool {
        matches!(
            node.schema_type,
            Some(SchemaType::Single(PrimitiveType::Object))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AdditionalProperties, PrimitiveType, SchemaNode, SchemaType};
    use indexmap::IndexMap;

    fn empty_defs() -> IndexMap<String, SchemaNode> {
        IndexMap::new()
    }

    #[test]
    fn simple_struct() {
        let root = SchemaNode {
            properties: IndexMap::from([
                (
                    "name".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    },
                ),
                (
                    "label".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    },
                ),
            ]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn required_vs_optional() {
        let root = SchemaNode {
            properties: IndexMap::from([
                (
                    "host".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    },
                ),
                (
                    "port".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::Integer)),
                        ..SchemaNode::default()
                    },
                ),
            ]),
            required: vec!["host".to_string()],
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Server");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn deny_unknown_fields() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "name".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                    ..SchemaNode::default()
                },
            )]),
            additional_properties: Some(AdditionalProperties::Bool(false)),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Strict");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn string_enum() {
        let mut defs = IndexMap::new();
        defs.insert(
            "color".to_string(),
            SchemaNode {
                enum_values: vec![
                    serde_json::Value::String("a".to_string()),
                    serde_json::Value::String("b".to_string()),
                    serde_json::Value::String("c".to_string()),
                ],
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode::default();
        let result = lower(&defs, &root, "Root");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn bool_mixed_enum() {
        let mut defs = IndexMap::new();
        defs.insert(
            "gpu".to_string(),
            SchemaNode {
                enum_values: vec![
                    serde_json::Value::Bool(true),
                    serde_json::Value::Bool(false),
                    serde_json::Value::String("auto".to_string()),
                ],
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode::default();
        let result = lower(&defs, &root, "Root");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn one_of_typed_variants() {
        let mut defs = IndexMap::new();
        defs.insert(
            "alpha".to_string(),
            SchemaNode {
                properties: IndexMap::from([(
                    "x".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    },
                )]),
                ..SchemaNode::default()
            },
        );
        defs.insert(
            "beta".to_string(),
            SchemaNode {
                properties: IndexMap::from([(
                    "y".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::Integer)),
                        ..SchemaNode::default()
                    },
                )]),
                ..SchemaNode::default()
            },
        );
        defs.insert(
            "choice".to_string(),
            SchemaNode {
                one_of: vec![
                    SchemaNode {
                        r#ref: Some("#/definitions/alpha".to_string()),
                        ..SchemaNode::default()
                    },
                    SchemaNode {
                        r#ref: Some("#/definitions/beta".to_string()),
                        ..SchemaNode::default()
                    },
                ],
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode::default();
        let result = lower(&defs, &root, "Root");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn any_of_variants() {
        let mut defs = IndexMap::new();
        defs.insert(
            "mixed".to_string(),
            SchemaNode {
                any_of: vec![
                    SchemaNode {
                        r#ref: Some("#/definitions/optA".to_string()),
                        ..SchemaNode::default()
                    },
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    },
                ],
                ..SchemaNode::default()
            },
        );
        defs.insert(
            "optA".to_string(),
            SchemaNode {
                properties: IndexMap::from([(
                    "val".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::Integer)),
                        ..SchemaNode::default()
                    },
                )]),
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode::default();
        let result = lower(&defs, &root, "Root");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn all_of_composition() {
        let mut defs = IndexMap::new();
        defs.insert(
            "base".to_string(),
            SchemaNode {
                properties: IndexMap::from([(
                    "id".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::Integer)),
                        ..SchemaNode::default()
                    },
                )]),
                ..SchemaNode::default()
            },
        );
        defs.insert(
            "combined".to_string(),
            SchemaNode {
                all_of: vec![
                    SchemaNode {
                        r#ref: Some("#/definitions/base".to_string()),
                        ..SchemaNode::default()
                    },
                    SchemaNode {
                        properties: IndexMap::from([(
                            "extra".to_string(),
                            SchemaNode {
                                schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                                ..SchemaNode::default()
                            },
                        )]),
                        ..SchemaNode::default()
                    },
                ],
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode::default();
        let result = lower(&defs, &root, "Root");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn multi_type_string_null() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "maybe".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Multi(vec![
                        PrimitiveType::String,
                        PrimitiveType::Null,
                    ])),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn multi_type_string_integer() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "value".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Multi(vec![
                        PrimitiveType::String,
                        PrimitiveType::Integer,
                    ])),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn array_with_items() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "tags".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::Array)),
                    items: Some(Box::new(SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    })),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn array_without_items() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "data".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::Array)),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn object_with_additional_properties_schema() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "env".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
                    additional_properties: Some(AdditionalProperties::Schema(Box::new(
                        SchemaNode {
                            schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                            ..SchemaNode::default()
                        },
                    ))),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn plain_object() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "metadata".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::Object)),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn ref_in_property() {
        let mut defs = IndexMap::new();
        defs.insert(
            "theme".to_string(),
            SchemaNode {
                enum_values: vec![
                    serde_json::Value::String("light".to_string()),
                    serde_json::Value::String("dark".to_string()),
                ],
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode {
            properties: IndexMap::from([(
                "theme".to_string(),
                SchemaNode {
                    r#ref: Some("#/definitions/theme".to_string()),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&defs, &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn nested_inline_object() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "server".to_string(),
                SchemaNode {
                    properties: IndexMap::from([
                        (
                            "host".to_string(),
                            SchemaNode {
                                schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                                ..SchemaNode::default()
                            },
                        ),
                        (
                            "port".to_string(),
                            SchemaNode {
                                schema_type: Some(SchemaType::Single(PrimitiveType::Integer)),
                                ..SchemaNode::default()
                            },
                        ),
                    ]),
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn deprecated_field() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "old_setting".to_string(),
                SchemaNode {
                    schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                    deprecated: true,
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn alias_for_trivial_definition() {
        let mut defs = IndexMap::new();
        defs.insert(
            "anything".to_string(),
            SchemaNode {
                description: Some("A free-form value".to_string()),
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode::default();
        let result = lower(&defs, &root, "Root");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn inline_enum_in_property() {
        let root = SchemaNode {
            properties: IndexMap::from([(
                "mode".to_string(),
                SchemaNode {
                    enum_values: vec![
                        serde_json::Value::String("fast".to_string()),
                        serde_json::Value::String("slow".to_string()),
                    ],
                    ..SchemaNode::default()
                },
            )]),
            ..SchemaNode::default()
        };
        let result = lower(&empty_defs(), &root, "Config");
        insta::assert_debug_snapshot!(result);
    }

    #[test]
    fn root_with_definitions() {
        let mut defs = IndexMap::new();
        defs.insert(
            "level".to_string(),
            SchemaNode {
                enum_values: vec![
                    serde_json::Value::String("info".to_string()),
                    serde_json::Value::String("warn".to_string()),
                ],
                ..SchemaNode::default()
            },
        );
        let root = SchemaNode {
            properties: IndexMap::from([
                (
                    "name".to_string(),
                    SchemaNode {
                        schema_type: Some(SchemaType::Single(PrimitiveType::String)),
                        ..SchemaNode::default()
                    },
                ),
                (
                    "log_level".to_string(),
                    SchemaNode {
                        r#ref: Some("#/definitions/level".to_string()),
                        ..SchemaNode::default()
                    },
                ),
            ]),
            required: vec!["name".to_string()],
            ..SchemaNode::default()
        };
        let result = lower(&defs, &root, "AppConfig");
        insta::assert_debug_snapshot!(result);
    }
}
