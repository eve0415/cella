use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::CodegenConfig;
use crate::ir::{EnumRepr, IrAlias, IrEnum, IrField, IrStruct, IrType, IrTypeRef, IrVariant};

pub fn emit_type(ir: &IrType, config: &CodegenConfig) -> TokenStream {
    match ir {
        IrType::Struct(s) => emit_struct(s, config),
        IrType::Enum(e) => emit_enum(e, config),
        IrType::Alias(a) => emit_alias(a, config),
    }
}

fn emit_struct(s: &IrStruct, config: &CodegenConfig) -> TokenStream {
    let name = format_ident!("{}", s.name);
    let doc = emit_doc(s.doc.as_ref(), config);

    let fields = s.fields.iter().map(|f| emit_field(f, config));

    quote! {
        #doc
        #[derive(Debug, Clone)]
        pub struct #name {
            #(#fields)*
        }
    }
}

fn emit_enum(e: &IrEnum, config: &CodegenConfig) -> TokenStream {
    let name = format_ident!("{}", e.name);
    let doc = emit_doc(e.doc.as_ref(), config);

    let variants = e.variants.iter().map(|v| emit_variant(v, &e.repr, config));

    quote! {
        #doc
        #[derive(Debug, Clone)]
        pub enum #name {
            #(#variants)*
        }
    }
}

fn emit_alias(a: &IrAlias, config: &CodegenConfig) -> TokenStream {
    let name = format_ident!("{}", a.name);
    let doc = emit_doc(a.doc.as_ref(), config);
    let ty = emit_type_ref(&a.ty);

    quote! {
        #doc
        pub type #name = #ty;
    }
}

fn emit_field(f: &IrField, config: &CodegenConfig) -> TokenStream {
    let name = format_ident!("{}", f.name);
    let doc = emit_doc(f.doc.as_ref(), config);
    let depr = if f.deprecated && config.emit_deprecated {
        quote! { #[deprecated] }
    } else {
        quote! {}
    };

    let ty = if f.required {
        emit_type_ref(&f.ty)
    } else {
        let inner = emit_type_ref(&f.ty);
        quote! { Option<#inner> }
    };

    quote! {
        #doc
        #depr
        pub #name: #ty,
    }
}

fn emit_variant(v: &IrVariant, repr: &EnumRepr, config: &CodegenConfig) -> TokenStream {
    let name = format_ident!("{}", v.name);
    let doc = emit_doc(v.doc.as_ref(), config);

    match repr {
        EnumRepr::StringEnum | EnumRepr::BoolMixed => {
            // Unit variant
            quote! { #doc #name, }
        }
        EnumRepr::TypedVariants | EnumRepr::MultiType => v.ty.as_ref().map_or_else(
            || quote! { #doc #name, },
            |ty| {
                let ty_tokens = emit_type_ref(ty);
                quote! { #doc #name(#ty_tokens), }
            },
        ),
    }
}

pub fn emit_type_ref(ty: &IrTypeRef) -> TokenStream {
    match ty {
        IrTypeRef::String => quote! { String },
        IrTypeRef::I64 => quote! { i64 },
        IrTypeRef::F64 => quote! { f64 },
        IrTypeRef::Bool => quote! { bool },
        IrTypeRef::Vec(inner) => {
            let inner_ty = emit_type_ref(inner);
            quote! { Vec<#inner_ty> }
        }
        IrTypeRef::Option(inner) => {
            let inner_ty = emit_type_ref(inner);
            quote! { Option<#inner_ty> }
        }
        IrTypeRef::Map(k, v) => {
            let k_ty = emit_type_ref(k);
            let v_ty = emit_type_ref(v);
            quote! { HashMap<#k_ty, #v_ty> }
        }
        IrTypeRef::Named(name) => {
            let ident = format_ident!("{}", name);
            quote! { #ident }
        }
        IrTypeRef::Value => quote! { serde_json::Value },
    }
}

fn emit_doc(doc: Option<&String>, config: &CodegenConfig) -> TokenStream {
    if config.emit_docs
        && let Some(text) = doc
    {
        return quote! { #[doc = #text] };
    }
    quote! {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::*;

    #[allow(clippy::needless_pass_by_value)]
    fn fmt(tokens: proc_macro2::TokenStream) -> String {
        let raw = tokens.to_string();
        syn::parse_file(&raw)
            .map(|f| prettyplease::unparse(&f))
            .unwrap_or(raw)
    }

    fn default_config() -> CodegenConfig {
        CodegenConfig {
            root_type_name: "Test".into(),
            emit_docs: false,
            emit_deprecated: false,
        }
    }

    #[test]
    fn struct_basic() {
        let ir = IrType::Struct(IrStruct {
            name: "Foo".into(),
            doc: None,
            fields: vec![
                IrField {
                    name: "bar".into(),
                    json_name: "bar".into(),
                    doc: None,
                    ty: IrTypeRef::String,
                    required: true,
                    deprecated: false,
                },
                IrField {
                    name: "baz".into(),
                    json_name: "baz".into(),
                    doc: None,
                    ty: IrTypeRef::I64,
                    required: true,
                    deprecated: false,
                },
            ],
            deny_unknown_fields: false,
            is_all_of: false,
        });
        let tokens = emit_type(&ir, &default_config());
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn struct_with_doc() {
        let ir = IrType::Struct(IrStruct {
            name: "Documented".into(),
            doc: Some("This is a documented struct.".into()),
            fields: vec![IrField {
                name: "x".into(),
                json_name: "x".into(),
                doc: Some("Field doc.".into()),
                ty: IrTypeRef::Bool,
                required: true,
                deprecated: false,
            }],
            deny_unknown_fields: false,
            is_all_of: false,
        });
        let config = CodegenConfig {
            root_type_name: "Test".into(),
            emit_docs: true,
            emit_deprecated: false,
        };
        let tokens = emit_type(&ir, &config);
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn struct_deprecated_field() {
        let ir = IrType::Struct(IrStruct {
            name: "WithDeprecated".into(),
            doc: None,
            fields: vec![IrField {
                name: "old_field".into(),
                json_name: "oldField".into(),
                doc: None,
                ty: IrTypeRef::String,
                required: true,
                deprecated: true,
            }],
            deny_unknown_fields: false,
            is_all_of: false,
        });
        let config = CodegenConfig {
            root_type_name: "Test".into(),
            emit_docs: false,
            emit_deprecated: true,
        };
        let tokens = emit_type(&ir, &config);
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn struct_optional_field() {
        let ir = IrType::Struct(IrStruct {
            name: "OptionalFields".into(),
            doc: None,
            fields: vec![IrField {
                name: "maybe".into(),
                json_name: "maybe".into(),
                doc: None,
                ty: IrTypeRef::String,
                required: false,
                deprecated: false,
            }],
            deny_unknown_fields: false,
            is_all_of: false,
        });
        let tokens = emit_type(&ir, &default_config());
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn enum_string() {
        let ir = IrType::Enum(IrEnum {
            name: "Color".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "Red".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::String("red".into())),
                    ty: None,
                },
                IrVariant {
                    name: "Blue".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::String("blue".into())),
                    ty: None,
                },
            ],
            repr: EnumRepr::StringEnum,
        });
        let tokens = emit_type(&ir, &default_config());
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn enum_typed_variants() {
        let ir = IrType::Enum(IrEnum {
            name: "Item".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "Str".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::String),
                },
                IrVariant {
                    name: "Obj".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::Named("FooConfig".into())),
                },
            ],
            repr: EnumRepr::TypedVariants,
        });
        let tokens = emit_type(&ir, &default_config());
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn enum_multi_type() {
        let ir = IrType::Enum(IrEnum {
            name: "Mixed".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "Text".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::String),
                },
                IrVariant {
                    name: "Num".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::I64),
                },
            ],
            repr: EnumRepr::MultiType,
        });
        let tokens = emit_type(&ir, &default_config());
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn alias_type() {
        let ir = IrType::Alias(IrAlias {
            name: "Anything".into(),
            doc: None,
            ty: IrTypeRef::Value,
        });
        let tokens = emit_type(&ir, &default_config());
        insta::assert_snapshot!(fmt(tokens));
    }

    #[test]
    fn type_ref_string() {
        let tokens = emit_type_ref(&IrTypeRef::String);
        insta::assert_snapshot!(tokens.to_string(), @"String");
    }

    #[test]
    fn type_ref_vec() {
        let tokens = emit_type_ref(&IrTypeRef::Vec(Box::new(IrTypeRef::String)));
        insta::assert_snapshot!(tokens.to_string(), @"Vec < String >");
    }

    #[test]
    fn type_ref_map() {
        let tokens = emit_type_ref(&IrTypeRef::Map(
            Box::new(IrTypeRef::String),
            Box::new(IrTypeRef::Value),
        ));
        insta::assert_snapshot!(tokens.to_string(), @"HashMap < String , serde_json :: Value >");
    }
}
