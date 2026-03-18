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
