use thiserror::Error;

#[derive(Debug, Error)]
pub enum CellaCodegenError {
    #[error("failed to parse schema JSON: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("invalid schema: {0}")]
    Schema(String),

    #[error("code emission failed: {0}")]
    Emit(String),

    #[error("formatting failed: {0}")]
    Format(String),
}
