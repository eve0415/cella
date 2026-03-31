//! Template and feature option validation and default resolution.

use std::collections::HashMap;

use crate::TemplateOption;
use crate::error::TemplateError;

/// Resolve option values by merging user-provided values with defaults.
///
/// For each option declared in the template metadata:
/// - If the user provided a value, validate and use it.
/// - If the user did not provide a value, use the default.
///
/// # Errors
///
/// Returns [`TemplateError::InvalidOptionValue`] when a user-provided value
/// fails validation (e.g. not in an enum's allowed set, wrong type for a
/// boolean option).
pub fn resolve_options<S: std::hash::BuildHasher>(
    template_id: &str,
    declared: &HashMap<String, TemplateOption, S>,
    user_values: &HashMap<String, serde_json::Value, S>,
) -> Result<HashMap<String, serde_json::Value>, TemplateError> {
    let mut resolved = HashMap::with_capacity(declared.len());

    for (key, opt) in declared {
        let value = if let Some(user_val) = user_values.get(key) {
            validate_option_value(template_id, key, opt, user_val)?;
            user_val.clone()
        } else {
            opt.default.clone()
        };
        resolved.insert(key.clone(), value);
    }

    Ok(resolved)
}

/// Validate a single option value against its declaration.
fn validate_option_value(
    template_id: &str,
    key: &str,
    opt: &TemplateOption,
    value: &serde_json::Value,
) -> Result<(), TemplateError> {
    match opt.option_type.as_str() {
        "boolean" => validate_boolean(template_id, key, value),
        "string" => validate_string(template_id, key, opt, value),
        _ => Ok(()), // Unknown types: pass through without validation
    }
}

fn validate_boolean(
    template_id: &str,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), TemplateError> {
    // Accept both actual booleans and string representations
    match value {
        serde_json::Value::Bool(_) => Ok(()),
        serde_json::Value::String(s) if s == "true" || s == "false" => Ok(()),
        _ => Err(TemplateError::InvalidOptionValue {
            template_id: template_id.to_owned(),
            option: key.to_owned(),
            reason: format!("expected boolean, got {value}"),
        }),
    }
}

fn validate_string(
    template_id: &str,
    key: &str,
    opt: &TemplateOption,
    value: &serde_json::Value,
) -> Result<(), TemplateError> {
    let serde_json::Value::String(s) = value else {
        return Err(TemplateError::InvalidOptionValue {
            template_id: template_id.to_owned(),
            option: key.to_owned(),
            reason: format!("expected string, got {value}"),
        });
    };

    // Enum validation: value must be in the allowed set
    if let Some(allowed) = &opt.enum_values
        && !allowed.contains(s)
    {
        return Err(TemplateError::InvalidOptionValue {
            template_id: template_id.to_owned(),
            option: key.to_owned(),
            reason: format!(
                "value \"{s}\" is not allowed; expected one of: {}",
                allowed.join(", ")
            ),
        });
    }

    // Proposals are suggestions only — any string value is accepted.
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_string_option(default: &str, enum_values: Option<Vec<&str>>) -> TemplateOption {
        TemplateOption {
            option_type: "string".to_owned(),
            description: None,
            default: json!(default),
            proposals: None,
            enum_values: enum_values.map(|v| v.into_iter().map(String::from).collect()),
        }
    }

    fn make_string_option_with_proposals(default: &str, proposals: Vec<&str>) -> TemplateOption {
        TemplateOption {
            option_type: "string".to_owned(),
            description: None,
            default: json!(default),
            proposals: Some(proposals.into_iter().map(String::from).collect()),
            enum_values: None,
        }
    }

    fn make_boolean_option(default: bool) -> TemplateOption {
        TemplateOption {
            option_type: "boolean".to_owned(),
            description: None,
            default: json!(default),
            proposals: None,
            enum_values: None,
        }
    }

    // -----------------------------------------------------------------------
    // resolve_options: defaults
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_uses_defaults_when_no_user_values() {
        let mut declared = HashMap::new();
        declared.insert("variant".to_owned(), make_string_option("trixie", None));
        declared.insert("debug".to_owned(), make_boolean_option(false));

        let resolved = resolve_options("test", &declared, &HashMap::new()).unwrap();
        assert_eq!(resolved["variant"], json!("trixie"));
        assert_eq!(resolved["debug"], json!(false));
    }

    // -----------------------------------------------------------------------
    // resolve_options: user overrides
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_uses_user_values_when_provided() {
        let mut declared = HashMap::new();
        declared.insert("variant".to_owned(), make_string_option("trixie", None));

        let mut user = HashMap::new();
        user.insert("variant".to_owned(), json!("bookworm"));

        let resolved = resolve_options("test", &declared, &user).unwrap();
        assert_eq!(resolved["variant"], json!("bookworm"));
    }

    // -----------------------------------------------------------------------
    // validate: enum
    // -----------------------------------------------------------------------

    #[test]
    fn validate_enum_accepts_allowed_value() {
        let mut declared = HashMap::new();
        declared.insert(
            "variant".to_owned(),
            make_string_option("a", Some(vec!["a", "b", "c"])),
        );

        let mut user = HashMap::new();
        user.insert("variant".to_owned(), json!("b"));

        assert!(resolve_options("test", &declared, &user).is_ok());
    }

    #[test]
    fn validate_enum_rejects_disallowed_value() {
        let mut declared = HashMap::new();
        declared.insert(
            "variant".to_owned(),
            make_string_option("a", Some(vec!["a", "b", "c"])),
        );

        let mut user = HashMap::new();
        user.insert("variant".to_owned(), json!("z"));

        let err = resolve_options("test", &declared, &user).unwrap_err();
        assert!(matches!(err, TemplateError::InvalidOptionValue { .. }));
    }

    // -----------------------------------------------------------------------
    // validate: proposals (flexible, any value accepted)
    // -----------------------------------------------------------------------

    #[test]
    fn validate_proposals_accepts_any_string() {
        let mut declared = HashMap::new();
        declared.insert(
            "variant".to_owned(),
            make_string_option_with_proposals("trixie", vec!["trixie", "bookworm"]),
        );

        let mut user = HashMap::new();
        user.insert("variant".to_owned(), json!("custom-variant"));

        assert!(resolve_options("test", &declared, &user).is_ok());
    }

    // -----------------------------------------------------------------------
    // validate: boolean
    // -----------------------------------------------------------------------

    #[test]
    fn validate_boolean_accepts_true() {
        let mut declared = HashMap::new();
        declared.insert("flag".to_owned(), make_boolean_option(false));

        let mut user = HashMap::new();
        user.insert("flag".to_owned(), json!(true));

        assert!(resolve_options("test", &declared, &user).is_ok());
    }

    #[test]
    fn validate_boolean_accepts_string_true() {
        let mut declared = HashMap::new();
        declared.insert("flag".to_owned(), make_boolean_option(false));

        let mut user = HashMap::new();
        user.insert("flag".to_owned(), json!("true"));

        assert!(resolve_options("test", &declared, &user).is_ok());
    }

    #[test]
    fn validate_boolean_rejects_non_boolean() {
        let mut declared = HashMap::new();
        declared.insert("flag".to_owned(), make_boolean_option(false));

        let mut user = HashMap::new();
        user.insert("flag".to_owned(), json!(42));

        let err = resolve_options("test", &declared, &user).unwrap_err();
        assert!(matches!(err, TemplateError::InvalidOptionValue { .. }));
    }

    // -----------------------------------------------------------------------
    // validate: type mismatch (string option gets non-string)
    // -----------------------------------------------------------------------

    #[test]
    fn validate_string_rejects_non_string() {
        let mut declared = HashMap::new();
        declared.insert("variant".to_owned(), make_string_option("a", None));

        let mut user = HashMap::new();
        user.insert("variant".to_owned(), json!(123));

        let err = resolve_options("test", &declared, &user).unwrap_err();
        assert!(matches!(err, TemplateError::InvalidOptionValue { .. }));
    }
}
