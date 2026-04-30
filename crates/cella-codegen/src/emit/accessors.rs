use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use super::types::emit_type_ref;
use crate::CodegenConfig;
use crate::ir::{IrEnum, IrField, IrStruct, IrType, IrTypeRef};

/// Emit accessor methods for the root allOf enum (e.g. `DevContainer`).
///
/// Generates:
/// 1. `common()` — returns `&DevContainerCommon` from any variant
/// 2. Per-field convenience accessors that delegate to `common()`
pub fn emit_accessors(types: &[IrType], config: &CodegenConfig) -> TokenStream {
    let Some((root_enum, common_struct, wrapper_struct)) = find_root_pattern(types, config) else {
        return quote! {};
    };

    let root_name = format_ident!("{}", root_enum.name);
    let common_name = format_ident!("{}", common_struct.name);

    let common_accessor = emit_common_accessor(root_enum, &common_name, wrapper_struct);
    let field_accessors: Vec<_> = common_struct
        .fields
        .iter()
        .map(emit_field_accessor)
        .collect();

    quote! {
        impl #root_name {
            #common_accessor
            #(#field_accessors)*
        }
    }
}

/// Find the root enum → (wrapper struct with common field, common struct) pattern.
///
/// Pattern: an enum where one variant wraps a struct containing
/// `dev_container_common: <Common>`, and another variant IS `<Common>`.
fn find_root_pattern<'a>(
    types: &'a [IrType],
    config: &CodegenConfig,
) -> Option<(&'a IrEnum, &'a IrStruct, &'a IrStruct)> {
    let root_enum = types.iter().find_map(|t| match t {
        IrType::Enum(e) if e.name == config.root_type_name => Some(e),
        _ => None,
    })?;

    if root_enum.variants.len() != 2 {
        return None;
    }

    let common_variant_type = root_enum.variants.iter().find_map(|v| {
        v.ty.as_ref().and_then(|ty| match ty {
            IrTypeRef::Named(name) if name.ends_with("Common") => Some(name.as_str()),
            _ => None,
        })
    })?;

    let common_struct = types.iter().find_map(|t| match t {
        IrType::Struct(s) if s.name == common_variant_type => Some(s),
        _ => None,
    })?;

    let wrapper_type = root_enum.variants.iter().find_map(|v| {
        v.ty.as_ref().and_then(|ty| match ty {
            IrTypeRef::Named(name) if name != common_variant_type => Some(name.as_str()),
            _ => None,
        })
    })?;

    let wrapper_struct = types.iter().find_map(|t| match t {
        IrType::Struct(s) if s.name == wrapper_type => Some(s),
        _ => None,
    })?;

    let has_common_field = wrapper_struct
        .fields
        .iter()
        .any(|f| matches!(&f.ty, IrTypeRef::Named(name) if name == common_variant_type));

    if !has_common_field {
        return None;
    }

    Some((root_enum, common_struct, wrapper_struct))
}

fn emit_common_accessor(
    root_enum: &IrEnum,
    common_name: &proc_macro2::Ident,
    wrapper_struct: &IrStruct,
) -> TokenStream {
    let common_name_str = common_name.to_string();
    let common_field = wrapper_struct
        .fields
        .iter()
        .find(|f| matches!(&f.ty, IrTypeRef::Named(name) if *name == common_name_str))
        .expect("wrapper struct must have common field");
    let common_field_name = format_ident!("{}", common_field.name);

    let variant0_name = format_ident!("{}", root_enum.variants[0].name);
    let variant1_name = format_ident!("{}", root_enum.variants[1].name);

    let (wrapper_variant, common_variant) = if root_enum.variants[0]
        .ty
        .as_ref()
        .is_some_and(|ty| matches!(ty, IrTypeRef::Named(name) if name == &wrapper_struct.name))
    {
        (&variant0_name, &variant1_name)
    } else {
        (&variant1_name, &variant0_name)
    };

    quote! {
        pub fn common(&self) -> &#common_name {
            match self {
                Self::#wrapper_variant(v) => &v.#common_field_name,
                Self::#common_variant(c) => c,
            }
        }
    }
}

fn emit_field_accessor(field: &IrField) -> TokenStream {
    let accessor_name = format_ident!("{}", field.name);

    if !field.required {
        return emit_optional_accessor(&accessor_name, field);
    }

    let ret_ty = emit_type_ref(&field.ty);
    quote! {
        pub fn #accessor_name(&self) -> &#ret_ty {
            &self.common().#accessor_name
        }
    }
}

fn emit_optional_accessor(accessor_name: &proc_macro2::Ident, field: &IrField) -> TokenStream {
    match &field.ty {
        IrTypeRef::String => {
            quote! {
                pub fn #accessor_name(&self) -> Option<&str> {
                    self.common().#accessor_name.as_deref()
                }
            }
        }
        IrTypeRef::Bool => {
            quote! {
                pub fn #accessor_name(&self) -> Option<bool> {
                    self.common().#accessor_name
                }
            }
        }
        IrTypeRef::I64 => {
            quote! {
                pub fn #accessor_name(&self) -> Option<i64> {
                    self.common().#accessor_name
                }
            }
        }
        IrTypeRef::F64 => {
            quote! {
                pub fn #accessor_name(&self) -> Option<f64> {
                    self.common().#accessor_name
                }
            }
        }
        IrTypeRef::Vec(inner) => {
            let inner_ty = emit_type_ref(inner);
            quote! {
                pub fn #accessor_name(&self) -> Option<&[#inner_ty]> {
                    self.common().#accessor_name.as_deref()
                }
            }
        }
        _ => {
            let inner_ty = emit_type_ref(&field.ty);
            quote! {
                pub fn #accessor_name(&self) -> Option<&#inner_ty> {
                    self.common().#accessor_name.as_ref()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::test_utils::fmt;
    use crate::ir::*;

    fn default_config() -> CodegenConfig {
        CodegenConfig {
            root_type_name: "Root".into(),
            emit_docs: false,
            emit_deprecated: false,
        }
    }

    fn make_root_types() -> Vec<IrType> {
        vec![
            IrType::Struct(IrStruct {
                name: "RootCommon".into(),
                doc: None,
                fields: vec![
                    IrField {
                        name: "name".into(),
                        json_name: "name".into(),
                        doc: None,
                        ty: IrTypeRef::String,
                        required: false,
                        deprecated: false,
                    },
                    IrField {
                        name: "privileged".into(),
                        json_name: "privileged".into(),
                        doc: None,
                        ty: IrTypeRef::Bool,
                        required: false,
                        deprecated: false,
                    },
                    IrField {
                        name: "mounts".into(),
                        json_name: "mounts".into(),
                        doc: None,
                        ty: IrTypeRef::Vec(Box::new(IrTypeRef::String)),
                        required: false,
                        deprecated: false,
                    },
                    IrField {
                        name: "features".into(),
                        json_name: "features".into(),
                        doc: None,
                        ty: IrTypeRef::Named("Features".into()),
                        required: false,
                        deprecated: false,
                    },
                    IrField {
                        name: "remote_env".into(),
                        json_name: "remoteEnv".into(),
                        doc: None,
                        ty: IrTypeRef::Map(
                            Box::new(IrTypeRef::String),
                            Box::new(IrTypeRef::Option(Box::new(IrTypeRef::String))),
                        ),
                        required: false,
                        deprecated: false,
                    },
                ],
                deny_unknown_fields: false,
                is_all_of: false,
            }),
            IrType::Struct(IrStruct {
                name: "RootVariant0".into(),
                doc: None,
                fields: vec![
                    IrField {
                        name: "part_0".into(),
                        json_name: "part_0".into(),
                        doc: None,
                        ty: IrTypeRef::Named("RootVariant0Part0".into()),
                        required: true,
                        deprecated: false,
                    },
                    IrField {
                        name: "root_common".into(),
                        json_name: "root_common".into(),
                        doc: None,
                        ty: IrTypeRef::Named("RootCommon".into()),
                        required: true,
                        deprecated: false,
                    },
                ],
                deny_unknown_fields: false,
                is_all_of: true,
            }),
            IrType::Enum(IrEnum {
                name: "Root".into(),
                doc: None,
                variants: vec![
                    IrVariant {
                        name: "RootVariant0".into(),
                        doc: None,
                        json_value: None,
                        ty: Some(IrTypeRef::Named("RootVariant0".into())),
                    },
                    IrVariant {
                        name: "RootCommon".into(),
                        doc: None,
                        json_value: None,
                        ty: Some(IrTypeRef::Named("RootCommon".into())),
                    },
                ],
                repr: EnumRepr::TypedVariants,
            }),
        ]
    }

    #[test]
    fn root_accessors() {
        let types = make_root_types();
        let tokens = emit_accessors(&types, &default_config());
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn no_accessors_for_non_root() {
        let types = vec![IrType::Struct(IrStruct {
            name: "NotRoot".into(),
            doc: None,
            fields: vec![],
            deny_unknown_fields: false,
            is_all_of: false,
        })];
        let tokens = emit_accessors(&types, &default_config());
        assert!(tokens.is_empty());
    }
}
