use proc_macro2::TokenStream;

use crate::CellaCodegenError;

/// Format a token stream into a pretty-printed Rust source string.
pub fn format_tokens(tokens: &TokenStream) -> Result<String, CellaCodegenError> {
    let raw = tokens.to_string();
    let parsed = syn::parse_file(&raw)
        .map_err(|e| CellaCodegenError::Format(format!("generated code is not valid Rust: {e}")))?;
    Ok(prettyplease::unparse(&parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    #[test]
    fn valid_tokens() {
        let tokens = quote! { pub struct Foo { pub x: i32 } };
        let result = format_tokens(&tokens).unwrap();
        insta::assert_snapshot!(result);
    }

    #[test]
    fn invalid_tokens() {
        // Emit tokens that are syntactically invalid Rust (missing struct name).
        let tokens = quote! { struct { } };
        let result = format_tokens(&tokens);
        assert!(result.is_err(), "should fail on invalid tokens");
    }

    #[test]
    fn empty_tokens() {
        let tokens = TokenStream::new();
        let result = format_tokens(&tokens).unwrap();
        assert!(result.is_empty() || result.trim().is_empty());
    }
}
