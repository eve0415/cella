/// Parse orchestrator: JSONC → `serde_json::Value` → validate → typed config.
use crate::diagnostic::{ConfigDiagnostic, ConfigDiagnostics, Severity};
use crate::jsonc;
use crate::schema;
use crate::span::SourceText;

/// Parse and validate a devcontainer.json file.
///
/// Returns the validated config and any warnings on success.
/// On failure, returns diagnostics with source-positioned errors.
///
/// # Errors
///
/// Returns `ConfigDiagnostics` if the input contains syntax errors or fails
/// schema validation.
pub fn parse_devcontainer(
    source_name: &str,
    raw_text: &str,
    strict: bool,
) -> Result<(schema::DevContainer, Vec<ConfigDiagnostic>), ConfigDiagnostics> {
    // Step 1: Strip JSONC
    let cleaned = match jsonc::strip_jsonc(raw_text) {
        Ok(c) => c,
        Err(e) => {
            let source = SourceText::new(source_name.into(), raw_text.into(), raw_text.into());
            return Err(ConfigDiagnostics::new(
                source,
                vec![ConfigDiagnostic {
                    severity: Severity::Error,
                    message: e.to_string(),
                    path: String::new(),
                    span: None,
                    help: Some("Check for unterminated comments".into()),
                }],
            ));
        }
    };

    let source = SourceText::new(source_name.into(), raw_text.into(), cleaned.clone());

    // Step 2: Parse JSON
    let value: serde_json::Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(e) => {
            return Err(ConfigDiagnostics::new(
                source,
                vec![ConfigDiagnostic {
                    severity: Severity::Error,
                    message: format!("JSON syntax error: {e}"),
                    path: String::new(),
                    span: None,
                    help: Some("Check for missing commas, brackets, or quotes".into()),
                }],
            ));
        }
    };

    // Step 3: Validate against schema
    match schema::DevContainer::validate(&value, "") {
        Ok(config) => Ok((config, Vec::new())),
        Err(validation_errors) => {
            let mut diagnostics: Vec<ConfigDiagnostic> = validation_errors
                .into_iter()
                .map(|ve| {
                    let path_segments: Vec<&str> =
                        ve.path.split('/').filter(|s| !s.is_empty()).collect();

                    let span = source.find_value_span(&path_segments);

                    let severity =
                        if ve.kind == schema::ValidationErrorKind::UnknownField && !strict {
                            Severity::Warning
                        } else {
                            Severity::Error
                        };

                    let help = match ve.kind {
                        schema::ValidationErrorKind::MissingRequired => Some(format!(
                            "add the required field \"{}\"",
                            ve.path.split('/').next_back().unwrap_or("")
                        )),
                        schema::ValidationErrorKind::UnknownField => {
                            Some("remove this field or check the spelling".into())
                        }
                        schema::ValidationErrorKind::InvalidEnumValue => {
                            Some("check the allowed values in the schema".into())
                        }
                        _ => None,
                    };

                    ConfigDiagnostic {
                        severity,
                        message: ve.message,
                        path: ve.path,
                        span,
                        help,
                    }
                })
                .collect();

            // In strict mode, upgrade warnings to errors
            if strict {
                for d in &mut diagnostics {
                    d.severity = Severity::Error;
                }
            }

            Err(ConfigDiagnostics::new(source, diagnostics))
        }
    }
}
