use super::Lowerer;
use crate::ir::naming::to_rust_type_name;
use crate::ir::{EnumRepr, IrEnum, IrField, IrStruct, IrType, IrTypeRef, IrVariant};
use crate::schema::{AdditionalProperties, PrimitiveType, SchemaNode};

use crate::ir::naming::to_rust_field_name;

impl Lowerer {
    // ── Struct ───────────────────────────────────────────────────────────

    pub(super) fn lower_struct(&mut self, name: &str, node: &SchemaNode) -> IrType {
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

    pub(super) fn lower_string_enum(name: &str, node: &SchemaNode) -> IrType {
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
                    serde_json::Value::String(s) => crate::ir::naming::to_variant_name(s),
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

    // ── Single type ─────────────────────────────────────────────────────

    pub(super) fn lower_single_type(
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
                    let ir = Self::lower_string_enum(context_name, node);
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

    // ── Object type ─────────────────────────────────────────────────────

    pub(super) fn lower_object_type(&mut self, node: &SchemaNode, context_name: &str) -> IrTypeRef {
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

    // ── Multi type ──────────────────────────────────────────────────────

    pub(super) fn lower_multi_type(
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
            // Multiple non-null types -> generate multi-type enum
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
}
