use std::collections::HashMap;

use crate::error::FeatureWarning;
use crate::types::{FeatureOption, OptionType};

/// Validate user-provided options against declared feature options.
///
/// All validation is advisory -- options are always passed through regardless.
/// Returns warnings for:
/// - Unknown option keys not declared in the feature metadata
/// - Type mismatches (e.g., string value for a boolean option)
/// - Enum values not in the declared allowed set
#[allow(clippy::implicit_hasher)]
pub fn validate_options(
    feature_id: &str,
    user_options: &HashMap<String, serde_json::Value>,
    declared_options: &HashMap<String, FeatureOption>,
) -> Vec<FeatureWarning> {
    let mut warnings = Vec::new();

    for (key, value) in user_options {
        let Some(decl) = declared_options.get(key) else {
            warnings.push(FeatureWarning::UnknownOption {
                feature_id: feature_id.to_string(),
                option: key.clone(),
            });
            continue;
        };

        // Type checking.
        match decl.option_type {
            OptionType::Boolean => {
                if !value.is_boolean() {
                    // Strings "true"/"false" are commonly accepted, but flag anything else.
                    let is_bool_string = value.as_str().is_some_and(|s| {
                        s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false")
                    });
                    if !is_bool_string {
                        warnings.push(FeatureWarning::TypeMismatch {
                            feature_id: feature_id.to_string(),
                            option: key.clone(),
                            expected: "boolean".to_string(),
                            got: value_type_name(value),
                        });
                    }
                }
            }
            OptionType::String => {
                // Strings accept anything that can be stringified, but check enum constraints.
                if let Some(enum_values) = &decl.enum_values {
                    let str_val = value_as_string(value);
                    if !enum_values.contains(&str_val) {
                        warnings.push(FeatureWarning::InvalidEnumValue {
                            feature_id: feature_id.to_string(),
                            option: key.clone(),
                            value: str_val,
                            allowed: enum_values.clone(),
                        });
                    }
                }
            }
        }
    }

    warnings
}

/// Get a human-readable type name for a JSON value.
fn value_type_name(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "boolean".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::String(_) => "string".to_string(),
        serde_json::Value::Array(_) => "array".to_string(),
        serde_json::Value::Object(_) => "object".to_string(),
    }
}

/// Coerce a JSON value to its string representation for enum comparison.
fn value_as_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_option_warns() {
        let declared = HashMap::new();
        let user = HashMap::from([("mystery".to_string(), json!("value"))]);

        let warnings = validate_options("test-feature", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::UnknownOption { feature_id, option } => {
                assert_eq!(feature_id, "test-feature");
                assert_eq!(option, "mystery");
            }
            other => panic!("expected UnknownOption, got {other:?}"),
        }
    }

    #[test]
    fn type_mismatch_boolean_gets_number() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);
        let user = HashMap::from([("flag".to_string(), json!(42))]);

        let warnings = validate_options("feat", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::TypeMismatch { expected, got, .. } => {
                assert_eq!(expected, "boolean");
                assert_eq!(got, "number");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn boolean_string_true_false_accepted() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);

        // "true" as string is accepted.
        let user = HashMap::from([("flag".to_string(), json!("true"))]);
        let warnings = validate_options("feat", &user, &declared);
        assert!(warnings.is_empty());

        // "false" as string is accepted.
        let user = HashMap::from([("flag".to_string(), json!("false"))]);
        let warnings = validate_options("feat", &user, &declared);
        assert!(warnings.is_empty());

        // "TRUE" case-insensitive.
        let user = HashMap::from([("flag".to_string(), json!("TRUE"))]);
        let warnings = validate_options("feat", &user, &declared);
        assert!(warnings.is_empty());
    }

    #[test]
    fn boolean_option_with_non_bool_string_warns() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);
        let user = HashMap::from([("flag".to_string(), json!("yes"))]);

        let warnings = validate_options("feat", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::TypeMismatch { expected, got, .. } => {
                assert_eq!(expected, "boolean");
                assert_eq!(got, "string");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn enum_value_not_in_allowed_set() {
        let declared = HashMap::from([(
            "version".to_string(),
            FeatureOption {
                option_type: OptionType::String,
                default: json!("lts"),
                description: None,
                enum_values: Some(vec![
                    "lts".to_string(),
                    "latest".to_string(),
                    "18".to_string(),
                ]),
            },
        )]);
        let user = HashMap::from([("version".to_string(), json!("99"))]);

        let warnings = validate_options("node", &user, &declared);

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            FeatureWarning::InvalidEnumValue { value, allowed, .. } => {
                assert_eq!(value, "99");
                assert_eq!(allowed, &vec!["lts", "latest", "18"]);
            }
            other => panic!("expected InvalidEnumValue, got {other:?}"),
        }
    }

    #[test]
    fn enum_value_in_allowed_set_no_warning() {
        let declared = HashMap::from([(
            "version".to_string(),
            FeatureOption {
                option_type: OptionType::String,
                default: json!("lts"),
                description: None,
                enum_values: Some(vec!["lts".to_string(), "latest".to_string()]),
            },
        )]);
        let user = HashMap::from([("version".to_string(), json!("lts"))]);

        let warnings = validate_options("node", &user, &declared);
        assert!(warnings.is_empty());
    }

    #[test]
    fn valid_options_no_warnings() {
        let declared = HashMap::from([
            (
                "version".to_string(),
                FeatureOption {
                    option_type: OptionType::String,
                    default: json!("lts"),
                    description: None,
                    enum_values: None,
                },
            ),
            (
                "install_tools".to_string(),
                FeatureOption {
                    option_type: OptionType::Boolean,
                    default: json!(true),
                    description: None,
                    enum_values: None,
                },
            ),
        ]);
        let user = HashMap::from([
            ("version".to_string(), json!("18")),
            ("install_tools".to_string(), json!(false)),
        ]);

        let warnings = validate_options("node", &user, &declared);
        assert!(warnings.is_empty());
    }

    #[test]
    fn multiple_warnings_collected() {
        let declared = HashMap::from([(
            "flag".to_string(),
            FeatureOption {
                option_type: OptionType::Boolean,
                default: json!(false),
                description: None,
                enum_values: None,
            },
        )]);
        let user = HashMap::from([
            ("flag".to_string(), json!(42)),
            ("unknown1".to_string(), json!("x")),
            ("unknown2".to_string(), json!("y")),
        ]);

        let warnings = validate_options("feat", &user, &declared);

        // Should have 3 warnings: one TypeMismatch + two UnknownOption.
        assert_eq!(warnings.len(), 3);
    }
}
