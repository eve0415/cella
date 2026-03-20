use super::Lowerer;
use crate::ir::naming::{ref_to_def_name, to_rust_field_name, to_rust_type_name};
use crate::ir::{EnumRepr, IrEnum, IrField, IrStruct, IrType, IrTypeRef, IrVariant};
use crate::schema::SchemaNode;

impl Lowerer {
    // ── oneOf ────────────────────────────────────────────────────────────

    pub(super) fn lower_one_of(&mut self, name: &str, node: &SchemaNode) -> IrType {
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

    pub(super) fn lower_one_of_variant(
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

    pub(super) fn lower_all_of(
        &mut self,
        name: &str,
        members: &[SchemaNode],
        parent_node: &SchemaNode,
    ) -> IrType {
        let mut fields = Vec::new();

        for (i, member) in members.iter().enumerate() {
            let (field_name, field_ty) = if let Some(ref_path) = &member.r#ref {
                // $ref -> compose as named field
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
                // Complex member -> generate auxiliary type
                let part_name = format!("{name}Part{i}");
                let ty = self.lower_inline_to_named(&part_name, member);
                (format!("part_{i}"), ty)
            } else if !member.properties.is_empty() || self.is_object_type(member) {
                // Inline object -> generate auxiliary struct
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

    pub(super) fn lower_any_of(&mut self, name: &str, node: &SchemaNode) -> IrType {
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
}
