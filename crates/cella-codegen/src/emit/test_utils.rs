//! Shared test utilities for codegen emit modules.

use proc_macro2::TokenStream;

/// Pretty-format a `TokenStream` for snapshot comparison.
pub fn fmt(tokens: &TokenStream) -> String {
    let raw = tokens.to_string();
    syn::parse_file(&raw)
        .map(|f| prettyplease::unparse(&f))
        .unwrap_or(raw)
}
