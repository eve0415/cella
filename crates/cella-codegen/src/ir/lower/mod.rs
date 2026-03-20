mod composite;
mod primitives;

use indexmap::IndexMap;

use super::naming::{ref_to_def_name, to_rust_type_name};
use super::{IrAlias, IrType, IrTypeRef};
use crate::schema::{PrimitiveType, SchemaNode, SchemaType};

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

pub(super) struct Lowerer {
    pub(super) types: Vec<IrType>,
    #[allow(dead_code)]
    pub(super) definitions: IndexMap<String, SchemaNode>,
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

    // ── Type reference lowering ─────────────────────────────────────────

    pub(super) fn lower_type_ref(&mut self, node: &SchemaNode, context_name: &str) -> IrTypeRef {
        // $ref -> Named
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

        // Properties without explicit type -> object
        if !node.properties.is_empty() {
            let ir = self.lower_struct(context_name, node);
            self.types.push(ir);
            return IrTypeRef::Named(context_name.to_string());
        }

        IrTypeRef::Value
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Lower an inline schema to a named type and return a `Named` reference.
    pub(super) fn lower_inline_to_named(&mut self, name: &str, node: &SchemaNode) -> IrTypeRef {
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
            // Simple typed node -- return the type directly instead of wrapping
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
    pub(super) const fn is_object_type(&self, node: &SchemaNode) -> bool {
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
