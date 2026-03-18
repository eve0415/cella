mod emit;
mod error;
mod ir;
mod schema;

pub use error::CellaCodegenError;

/// Configuration for the code generator.
pub struct CodegenConfig {
    /// The name for the root generated type (e.g., `"DevContainer"`).
    pub root_type_name: String,
    /// Whether to emit `#[doc = "..."]` attributes from schema descriptions.
    pub emit_docs: bool,
    /// Whether to emit `#[deprecated]` attributes on deprecated schema fields.
    pub emit_deprecated: bool,
}

/// Generate Rust types and validators from a JSON Schema string.
///
/// Returns a formatted Rust source string suitable for `include!()`.
///
/// # Errors
///
/// Returns `CellaCodegenError` if parsing, lowering, emission, or formatting fails.
pub fn generate(schema_json: &str, config: &CodegenConfig) -> Result<String, CellaCodegenError> {
    let value: serde_json::Value = serde_json::from_str(schema_json)?;
    let parsed = schema::parse::parse_root_schema(&value)?;
    let ir_types = ir::lower::lower(&parsed.definitions, &parsed.root, &config.root_type_name);
    let tokens = emit::emit_all(&ir_types, config);
    emit::format::format_tokens(&tokens)
}
