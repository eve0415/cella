use proc_macro2::TokenStream;

use crate::CellaCodegenError;

/// Format a token stream into a pretty-printed Rust source string.
pub fn format_tokens(tokens: &TokenStream) -> Result<String, CellaCodegenError> {
    let raw = tokens.to_string();
    let parsed = syn::parse_file(&raw)
        .map_err(|e| CellaCodegenError::Format(format!("generated code is not valid Rust: {e}")))?;
    Ok(prettyplease::unparse(&parsed))
}
