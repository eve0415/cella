use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::ir::{EnumRepr, IrEnum, IrField, IrStruct, IrType, IrTypeRef, IrVariant};

/// Emit the shared validation infrastructure types.
pub fn emit_validation_infra() -> TokenStream {
    quote! {
        #[derive(Debug, Clone)]
        pub struct ValidationError {
            pub path: String,
            pub message: String,
            pub kind: ValidationErrorKind,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum ValidationErrorKind {
            TypeError,
            MissingRequired,
            UnknownField,
            InvalidEnumValue,
            MutuallyExclusive,
            OutOfRange,
            PatternMismatch,
            ConstraintViolation,
        }

        impl std::fmt::Display for ValidationError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}: {}", self.path, self.message)
            }
        }

        impl std::error::Error for ValidationError {}
    }
}

/// Emit a `validate()` impl for one IR type.
pub fn emit_validate(ir: &IrType) -> TokenStream {
    match ir {
        IrType::Struct(s) => {
            if s.is_all_of {
                emit_all_of_validate(s)
            } else {
                emit_struct_validate(s)
            }
        }
        IrType::Enum(e) => emit_enum_validate(e),
        IrType::Alias(_) => quote! {}, // aliases don't get validators
    }
}

// ── Struct validation ────────────────────────────────────────────────────

fn emit_struct_validate(s: &IrStruct) -> TokenStream {
    let name = format_ident!("{}", s.name);

    let required_checks: Vec<TokenStream> = s
        .fields
        .iter()
        .filter(|f| f.required)
        .map(|f| {
            let json_name = &f.json_name;
            quote! {
                if !obj.contains_key(#json_name) {
                    errors.push(ValidationError {
                        path: format!("{path}/{}", #json_name),
                        message: format!("missing required field: {}", #json_name),
                        kind: ValidationErrorKind::MissingRequired,
                    });
                }
            }
        })
        .collect();

    let field_validations: Vec<TokenStream> = s.fields.iter().map(emit_field_validation).collect();

    let unknown_check = if s.deny_unknown_fields {
        let known: Vec<&str> = s.fields.iter().map(|f| f.json_name.as_str()).collect();
        quote! {
            for key in obj.keys() {
                if ![#(#known),*].contains(&key.as_str()) {
                    errors.push(ValidationError {
                        path: format!("{path}/{key}"),
                        message: format!("unknown field: {key}"),
                        kind: ValidationErrorKind::UnknownField,
                    });
                }
            }
        }
    } else {
        quote! {}
    };

    let field_constructions: Vec<TokenStream> = s
        .fields
        .iter()
        .map(|f| {
            let field_ident = format_ident!("{}", f.name);
            if f.required {
                quote! { #field_ident: #field_ident.unwrap() }
            } else {
                quote! { #field_ident }
            }
        })
        .collect();

    quote! {
        impl #name {
            pub fn validate(value: &serde_json::Value, path: &str) -> Result<Self, Vec<ValidationError>> {
                let obj = value.as_object().ok_or_else(|| vec![ValidationError {
                    path: path.to_string(),
                    message: "expected object".to_string(),
                    kind: ValidationErrorKind::TypeError,
                }])?;
                let mut errors: Vec<ValidationError> = Vec::new();

                #(#required_checks)*
                #(#field_validations)*
                #unknown_check

                if errors.is_empty() {
                    Ok(Self {
                        #(#field_constructions,)*
                    })
                } else {
                    Err(errors)
                }
            }
        }
    }
}

fn emit_field_validation(f: &IrField) -> TokenStream {
    let field_ident = format_ident!("{}", f.name);
    let json_name = &f.json_name;
    let validate_expr = emit_value_validation(&f.ty, &quote! { v }, &quote! { field_path });

    // Both required and optional fields use the same extraction logic.
    // Required-ness is enforced by the separate required_checks above.
    quote! {
        let #field_ident = if let Some(v) = obj.get(#json_name) {
            let field_path = format!("{path}/{}", #json_name);
            match #validate_expr {
                Ok(val) => Some(val),
                Err(mut e) => { errors.append(&mut e); None }
            }
        } else {
            None
        };
    }
}

// ── AllOf struct validation ──────────────────────────────────────────────

fn emit_all_of_validate(s: &IrStruct) -> TokenStream {
    let name = format_ident!("{}", s.name);

    let member_validations: Vec<TokenStream> = s
        .fields
        .iter()
        .map(|f| {
            let field_ident = format_ident!("{}", f.name);
            let ty_ident = match &f.ty {
                IrTypeRef::Named(n) => format_ident!("{}", n),
                _ => format_ident!("serde_json_Value"),
            };

            match &f.ty {
                IrTypeRef::Named(_) => {
                    quote! {
                        let #field_ident = match #ty_ident::validate(value, path) {
                            Ok(v) => Some(v),
                            Err(mut e) => { errors.append(&mut e); None }
                        };
                    }
                }
                _ => {
                    quote! {
                        let #field_ident = Some(value.clone());
                    }
                }
            }
        })
        .collect();

    let field_constructions: Vec<TokenStream> = s
        .fields
        .iter()
        .map(|f| {
            let field_ident = format_ident!("{}", f.name);
            quote! { #field_ident: #field_ident.unwrap() }
        })
        .collect();

    quote! {
        impl #name {
            pub fn validate(value: &serde_json::Value, path: &str) -> Result<Self, Vec<ValidationError>> {
                let mut errors: Vec<ValidationError> = Vec::new();

                #(#member_validations)*

                if errors.is_empty() {
                    Ok(Self {
                        #(#field_constructions,)*
                    })
                } else {
                    Err(errors)
                }
            }
        }
    }
}

// ── Enum validation ──────────────────────────────────────────────────────

fn emit_enum_validate(e: &IrEnum) -> TokenStream {
    match e.repr {
        EnumRepr::StringEnum => emit_string_enum_validate(e),
        EnumRepr::BoolMixed => emit_bool_mixed_validate(e),
        EnumRepr::TypedVariants => emit_typed_variants_validate(e),
        EnumRepr::MultiType => emit_multi_type_validate(e),
    }
}

fn emit_string_enum_validate(e: &IrEnum) -> TokenStream {
    let name = format_ident!("{}", e.name);

    let match_arms: Vec<TokenStream> = e
        .variants
        .iter()
        .filter_map(|v| {
            let variant_ident = format_ident!("{}", v.name);
            if let Some(serde_json::Value::String(s)) = &v.json_value {
                Some(quote! { #s => Ok(Self::#variant_ident), })
            } else {
                None
            }
        })
        .collect();

    let valid_values: Vec<&str> = e
        .variants
        .iter()
        .filter_map(|v| {
            if let Some(serde_json::Value::String(s)) = &v.json_value {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect();
    let valid_list = valid_values.join(", ");

    quote! {
        impl #name {
            pub fn validate(value: &serde_json::Value, path: &str) -> Result<Self, Vec<ValidationError>> {
                let s = value.as_str().ok_or_else(|| vec![ValidationError {
                    path: path.to_string(),
                    message: "expected string".to_string(),
                    kind: ValidationErrorKind::TypeError,
                }])?;
                match s {
                    #(#match_arms)*
                    other => Err(vec![ValidationError {
                        path: path.to_string(),
                        message: format!("invalid value \"{other}\", expected one of: {}", #valid_list),
                        kind: ValidationErrorKind::InvalidEnumValue,
                    }]),
                }
            }
        }
    }
}

fn emit_bool_mixed_validate(e: &IrEnum) -> TokenStream {
    let name = format_ident!("{}", e.name);

    let mut arms = Vec::new();
    for v in &e.variants {
        let variant_ident = format_ident!("{}", v.name);
        match &v.json_value {
            Some(serde_json::Value::String(s)) => {
                arms.push(quote! {
                    if let Some(s) = value.as_str() {
                        if s == #s { return Ok(Self::#variant_ident); }
                    }
                });
            }
            Some(serde_json::Value::Bool(b)) => {
                arms.push(quote! {
                    if let Some(b) = value.as_bool() {
                        if b == #b { return Ok(Self::#variant_ident); }
                    }
                });
            }
            _ => {}
        }
    }

    quote! {
        impl #name {
            pub fn validate(value: &serde_json::Value, path: &str) -> Result<Self, Vec<ValidationError>> {
                #(#arms)*
                Err(vec![ValidationError {
                    path: path.to_string(),
                    message: "value does not match any expected variant".to_string(),
                    kind: ValidationErrorKind::InvalidEnumValue,
                }])
            }
        }
    }
}

fn emit_typed_variants_validate(e: &IrEnum) -> TokenStream {
    let name = format_ident!("{}", e.name);

    let try_arms: Vec<TokenStream> = e
        .variants
        .iter()
        .map(|v| emit_try_variant(&name, v))
        .collect();

    quote! {
        impl #name {
            pub fn validate(value: &serde_json::Value, path: &str) -> Result<Self, Vec<ValidationError>> {
                #(#try_arms)*
                Err(vec![ValidationError {
                    path: path.to_string(),
                    message: "value does not match any expected variant".to_string(),
                    kind: ValidationErrorKind::TypeError,
                }])
            }
        }
    }
}

fn emit_try_variant(enum_name: &proc_macro2::Ident, v: &IrVariant) -> TokenStream {
    let variant_ident = format_ident!("{}", v.name);

    match &v.ty {
        Some(IrTypeRef::Named(type_name)) => {
            let ty_ident = format_ident!("{}", type_name);
            quote! {
                if let Ok(val) = #ty_ident::validate(value, path) {
                    return Ok(#enum_name::#variant_ident(val));
                }
            }
        }
        Some(ty) => {
            let validate_expr =
                emit_value_validation(ty, &quote! { value }, &quote! { path.to_string() });
            quote! {
                if let Ok(val) = #validate_expr {
                    return Ok(#enum_name::#variant_ident(val));
                }
            }
        }
        None => {
            quote! {
                // Unit variant — no validation possible
            }
        }
    }
}

fn emit_multi_type_validate(e: &IrEnum) -> TokenStream {
    let name = format_ident!("{}", e.name);

    let type_checks: Vec<TokenStream> = e
        .variants
        .iter()
        .filter_map(|v| {
            let variant_ident = format_ident!("{}", v.name);
            let ty = v.ty.as_ref()?;
            let validate_expr =
                emit_value_validation(ty, &quote! { value }, &quote! { path.to_string() });

            let type_check = match ty {
                IrTypeRef::String => quote! { value.is_string() },
                IrTypeRef::I64 => quote! { value.is_i64() },
                IrTypeRef::F64 => quote! { value.is_f64() || value.is_i64() },
                IrTypeRef::Bool => quote! { value.is_boolean() },
                IrTypeRef::Vec(_) => quote! { value.is_array() },
                IrTypeRef::Map(_, _) | IrTypeRef::Named(_) => quote! { value.is_object() },
                IrTypeRef::Value | IrTypeRef::Option(_) => quote! { true },
            };

            Some(quote! {
                if #type_check {
                    if let Ok(val) = #validate_expr {
                        return Ok(#name::#variant_ident(val));
                    }
                }
            })
        })
        .collect();

    quote! {
        impl #name {
            pub fn validate(value: &serde_json::Value, path: &str) -> Result<Self, Vec<ValidationError>> {
                #(#type_checks)*
                Err(vec![ValidationError {
                    path: path.to_string(),
                    message: "value does not match any expected type".to_string(),
                    kind: ValidationErrorKind::TypeError,
                }])
            }
        }
    }
}

// ── Value validation expressions ─────────────────────────────────────────

fn emit_value_validation(ty: &IrTypeRef, value: &TokenStream, path: &TokenStream) -> TokenStream {
    match ty {
        IrTypeRef::String => quote! {
            #value.as_str().map(String::from).ok_or_else(|| vec![ValidationError {
                path: #path.to_string(),
                message: "expected string".to_string(),
                kind: ValidationErrorKind::TypeError,
            }])
        },
        IrTypeRef::I64 => quote! {
            #value.as_i64().ok_or_else(|| vec![ValidationError {
                path: #path.to_string(),
                message: "expected integer".to_string(),
                kind: ValidationErrorKind::TypeError,
            }])
        },
        IrTypeRef::F64 => quote! {
            #value.as_f64().ok_or_else(|| vec![ValidationError {
                path: #path.to_string(),
                message: "expected number".to_string(),
                kind: ValidationErrorKind::TypeError,
            }])
        },
        IrTypeRef::Bool => quote! {
            #value.as_bool().ok_or_else(|| vec![ValidationError {
                path: #path.to_string(),
                message: "expected boolean".to_string(),
                kind: ValidationErrorKind::TypeError,
            }])
        },
        IrTypeRef::Vec(inner) => {
            let inner_validate =
                emit_value_validation(inner, &quote! { item }, &quote! { item_path });
            quote! {
                (|| -> Result<_, Vec<ValidationError>> {
                    let arr = #value.as_array().ok_or_else(|| vec![ValidationError {
                        path: #path.to_string(),
                        message: "expected array".to_string(),
                        kind: ValidationErrorKind::TypeError,
                    }])?;
                    let mut result = Vec::new();
                    let mut errs: Vec<ValidationError> = Vec::new();
                    for (i, item) in arr.iter().enumerate() {
                        let item_path = format!("{}/{i}", #path);
                        match #inner_validate {
                            Ok(val) => result.push(val),
                            Err(mut e) => errs.append(&mut e),
                        }
                    }
                    if errs.is_empty() { Ok(result) } else { Err(errs) }
                })()
            }
        }
        IrTypeRef::Option(inner) => {
            let inner_validate = emit_value_validation(inner, value, path);
            quote! {
                if #value.is_null() {
                    Ok(None)
                } else {
                    (#inner_validate).map(Some)
                }
            }
        }
        IrTypeRef::Map(_, v) => {
            let value_validate =
                emit_value_validation(v, &quote! { map_val }, &quote! { entry_path });
            quote! {
                (|| -> Result<_, Vec<ValidationError>> {
                    let obj = #value.as_object().ok_or_else(|| vec![ValidationError {
                        path: #path.to_string(),
                        message: "expected object".to_string(),
                        kind: ValidationErrorKind::TypeError,
                    }])?;
                    let mut result = HashMap::new();
                    let mut errs: Vec<ValidationError> = Vec::new();
                    for (k, map_val) in obj {
                        let entry_path = format!("{}/{k}", #path);
                        match #value_validate {
                            Ok(val) => { result.insert(k.clone(), val); },
                            Err(mut e) => errs.append(&mut e),
                        }
                    }
                    if errs.is_empty() { Ok(result) } else { Err(errs) }
                })()
            }
        }
        IrTypeRef::Named(type_name) => {
            let ident = format_ident!("{}", type_name);
            quote! { #ident::validate(#value, &#path) }
        }
        IrTypeRef::Value => quote! {
            Ok::<serde_json::Value, Vec<ValidationError>>(#value.clone())
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::test_utils::fmt;
    use crate::ir::*;

    #[test]
    fn validation_infra() {
        let tokens = emit_validation_infra();
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn struct_validate_basic() {
        let ir = IrType::Struct(IrStruct {
            name: "Basic".into(),
            doc: None,
            fields: vec![IrField {
                name: "name".into(),
                json_name: "name".into(),
                doc: None,
                ty: IrTypeRef::String,
                required: true,
                deprecated: false,
            }],
            deny_unknown_fields: false,
            is_all_of: false,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn struct_validate_deny_unknown() {
        let ir = IrType::Struct(IrStruct {
            name: "Strict".into(),
            doc: None,
            fields: vec![IrField {
                name: "allowed".into(),
                json_name: "allowed".into(),
                doc: None,
                ty: IrTypeRef::String,
                required: true,
                deprecated: false,
            }],
            deny_unknown_fields: true,
            is_all_of: false,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn all_of_validate() {
        let ir = IrType::Struct(IrStruct {
            name: "Combined".into(),
            doc: None,
            fields: vec![
                IrField {
                    name: "base".into(),
                    json_name: "base".into(),
                    doc: None,
                    ty: IrTypeRef::Named("BaseConfig".into()),
                    required: true,
                    deprecated: false,
                },
                IrField {
                    name: "extra".into(),
                    json_name: "extra".into(),
                    doc: None,
                    ty: IrTypeRef::Named("ExtraConfig".into()),
                    required: true,
                    deprecated: false,
                },
            ],
            deny_unknown_fields: false,
            is_all_of: true,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn string_enum_validate() {
        let ir = IrType::Enum(IrEnum {
            name: "Status".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "Active".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::String("active".into())),
                    ty: None,
                },
                IrVariant {
                    name: "Inactive".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::String("inactive".into())),
                    ty: None,
                },
            ],
            repr: EnumRepr::StringEnum,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn bool_mixed_validate() {
        let ir = IrType::Enum(IrEnum {
            name: "Toggle".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "On".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::Bool(true)),
                    ty: None,
                },
                IrVariant {
                    name: "Off".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::Bool(false)),
                    ty: None,
                },
                IrVariant {
                    name: "Custom".into(),
                    doc: None,
                    json_value: Some(serde_json::Value::String("custom".into())),
                    ty: None,
                },
            ],
            repr: EnumRepr::BoolMixed,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn typed_variants_validate() {
        let ir = IrType::Enum(IrEnum {
            name: "Config".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "Alpha".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::Named("AlphaConfig".into())),
                },
                IrVariant {
                    name: "Beta".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::Named("BetaConfig".into())),
                },
            ],
            repr: EnumRepr::TypedVariants,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn multi_type_validate() {
        let ir = IrType::Enum(IrEnum {
            name: "Flexible".into(),
            doc: None,
            variants: vec![
                IrVariant {
                    name: "Text".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::String),
                },
                IrVariant {
                    name: "Number".into(),
                    doc: None,
                    json_value: None,
                    ty: Some(IrTypeRef::I64),
                },
            ],
            repr: EnumRepr::MultiType,
        });
        let tokens = emit_validate(&ir);
        insta::assert_snapshot!(fmt(&tokens));
    }

    #[test]
    fn alias_no_validator() {
        let ir = IrType::Alias(IrAlias {
            name: "Passthrough".into(),
            doc: None,
            ty: IrTypeRef::Value,
        });
        let tokens = emit_validate(&ir);
        assert!(tokens.is_empty(), "alias should produce no validator");
    }
}
