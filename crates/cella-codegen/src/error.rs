use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum CellaCodegenError {
    #[error("failed to parse schema JSON: {0}")]
    #[diagnostic(code(cella::codegen::json_parse))]
    JsonParse(#[from] serde_json::Error),

    #[error("invalid schema: {0}")]
    #[diagnostic(code(cella::codegen::schema))]
    Schema(String),

    #[error("code emission failed: {0}")]
    #[diagnostic(code(cella::codegen::emit))]
    Emit(String),

    #[error("formatting failed: {0}")]
    #[diagnostic(code(cella::codegen::format))]
    Format(String),
}
