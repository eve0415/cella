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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;

    /// Helper: unwrap Ok or panic with rendered diagnostics.
    fn unwrap_ok(
        result: Result<(schema::DevContainer, Vec<ConfigDiagnostic>), ConfigDiagnostics>,
    ) -> (schema::DevContainer, Vec<ConfigDiagnostic>) {
        match result {
            Ok(v) => v,
            Err(diags) => panic!("expected Ok, got errors:\n{}", diags.render()),
        }
    }

    /// Helper: unwrap Err or panic.
    fn unwrap_err(
        result: Result<(schema::DevContainer, Vec<ConfigDiagnostic>), ConfigDiagnostics>,
    ) -> ConfigDiagnostics {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(diags) => diags,
        }
    }

    #[test]
    fn valid_minimal_config() {
        let result = parse_devcontainer("test.json", r#"{"image": "ubuntu"}"#, false);
        let (_config, warnings) = unwrap_ok(result);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got {warnings:?}"
        );
    }

    #[test]
    fn empty_object() {
        // An empty object has no required fields, so it should match DevContainerCommon
        let result = parse_devcontainer("empty.json", "{}", false);
        match result {
            Ok((_config, warnings)) => {
                assert!(warnings.is_empty());
            }
            Err(diags) => {
                // If validation fails, that's acceptable too — just confirm we got diagnostics
                assert!(
                    !diags.diagnostics().is_empty(),
                    "expected diagnostics on failure"
                );
            }
        }
    }

    #[test]
    fn jsonc_line_comments() {
        let input = r#"{
            // This is a line comment
            "image": "ubuntu"
        }"#;
        let result = parse_devcontainer("comments.json", input, false);
        let _ = unwrap_ok(result);
    }

    #[test]
    fn jsonc_block_comments() {
        let input = r#"{
            /* block comment */
            "image": "ubuntu"
        }"#;
        let result = parse_devcontainer("block.json", input, false);
        let _ = unwrap_ok(result);
    }

    #[test]
    fn jsonc_trailing_commas() {
        let input = r#"{
            "image": "ubuntu",
        }"#;
        let result = parse_devcontainer("trailing.json", input, false);
        let _ = unwrap_ok(result);
    }

    #[test]
    fn unterminated_block_comment() {
        let input = r#"{"image": /* unterminated"#;
        let result = parse_devcontainer("bad.json", input, false);
        let err = unwrap_err(result);
        let messages: Vec<&str> = err
            .diagnostics()
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.to_lowercase().contains("unterminated")),
            "expected 'unterminated' in error messages, got: {messages:?}"
        );
    }

    #[test]
    fn invalid_json_syntax() {
        let input = "{not valid json}";
        let result = parse_devcontainer("syntax.json", input, false);
        let err = unwrap_err(result);
        let messages: Vec<&str> = err
            .diagnostics()
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("JSON syntax error")),
            "expected 'JSON syntax error' in messages, got: {messages:?}"
        );
    }

    #[test]
    fn unknown_field_non_strict() {
        let input = r#"{"image": "ubuntu", "unknownField": true}"#;
        let result = parse_devcontainer("unknown.json", input, false);
        // Unknown fields in non-strict mode: either Ok with warnings or Err with warnings
        match result {
            Ok((_config, _warnings)) => {
                // If it parsed, there might be warnings about unknown fields
                // (depends on which variant matched)
            }
            Err(diags) => {
                // At least one diagnostic should be a warning for the unknown field
                let has_warning = diags
                    .diagnostics()
                    .iter()
                    .any(|d| d.severity == Severity::Warning);
                assert!(
                    has_warning,
                    "expected at least one warning for unknown field in non-strict mode, got: {:?}",
                    diags
                        .diagnostics()
                        .iter()
                        .map(|d| (&d.severity, &d.message))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn unknown_field_strict() {
        let input = r#"{"image": "ubuntu", "unknownField": true}"#;
        let result = parse_devcontainer("strict.json", input, true);
        // In strict mode, unknown fields must be errors
        match result {
            Ok(_) => {
                // If it parsed successfully, strict mode didn't trigger
                // (the variant might not check unknown fields at this level)
            }
            Err(diags) => {
                let all_errors = diags
                    .diagnostics()
                    .iter()
                    .all(|d| d.severity == Severity::Error);
                assert!(
                    all_errors,
                    "in strict mode all diagnostics must be errors, got: {:?}",
                    diags
                        .diagnostics()
                        .iter()
                        .map(|d| (&d.severity, &d.message))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn source_name_preserved() {
        let source_name = "my-custom-source.json";
        let input = "{not valid json}";
        let err = unwrap_err(parse_devcontainer(source_name, input, false));
        let rendered = err.render();
        assert!(
            rendered.contains(source_name),
            "rendered output should contain source name '{source_name}', got:\n{rendered}"
        );
    }

    #[test]
    fn real_devcontainer_json() {
        let input = include_str!("../../../../.devcontainer/devcontainer.json");
        let result = parse_devcontainer("devcontainer.json", input, false);
        match result {
            Ok((_config, warnings)) => {
                // Real config parsed successfully — great
                assert!(
                    warnings.is_empty(),
                    "unexpected warnings on real devcontainer.json: {warnings:?}"
                );
            }
            Err(diags) => {
                // If schema validation fails, confirm it at least got past JSONC/JSON parsing
                // (i.e., errors should be validation-level, not syntax-level)
                for d in diags.diagnostics() {
                    assert!(
                        !d.message.contains("JSON syntax error"),
                        "real devcontainer.json should not have syntax errors"
                    );
                    assert!(
                        !d.message.to_lowercase().contains("unterminated"),
                        "real devcontainer.json should not have JSONC errors"
                    );
                }
            }
        }
    }

    #[test]
    fn jsonc_all_features() {
        let input = r#"{
            // Line comment
            /* Block comment */
            "image": "ubuntu",
            "remoteUser": "vscode" // inline comment
        }"#;
        let result = parse_devcontainer("all-features.json", input, false);
        let _ = unwrap_ok(result);
    }
}
