pub mod format;
#[cfg(test)]
mod test_utils;
pub mod types;
pub mod validate;

use proc_macro2::TokenStream;
use quote::quote;

use crate::CodegenConfig;
use crate::ir::IrType;

/// Emit all IR types as a complete Rust module.
pub fn emit_all(ir_types: &[IrType], config: &CodegenConfig) -> TokenStream {
    let preamble = emit_preamble();
    let infra = validate::emit_validation_infra();

    let mut type_tokens = Vec::new();
    let mut impl_tokens = Vec::new();

    for ir in ir_types {
        type_tokens.push(types::emit_type(ir, config));
        impl_tokens.push(validate::emit_validate(ir));
    }

    quote! {
        #preamble
        #infra
        #(#type_tokens)*
        #(#impl_tokens)*
    }
}

fn emit_preamble() -> TokenStream {
    quote! {
        use std::collections::HashMap;
    }
}
