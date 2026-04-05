//! Shared interactive prompt functions for feature and template option
//! configuration. Used by both `cella init` wizard and `cella features edit`.

use std::collections::HashMap;

use inquire::{Confirm, Select, Text};

use cella_templates::types::TemplateOption;

/// Prompt for a single option value.
///
/// Handles boolean (confirm), enum (select from list), proposals (select
/// with custom entry), and free-form text inputs.
///
/// # Errors
///
/// Returns error if the user cancels the prompt.
pub fn prompt_single_option(
    key: &str,
    opt: &TemplateOption,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let description = opt.description.as_deref().unwrap_or(key);

    match opt.option_type.as_str() {
        "boolean" => {
            let default = opt.default.as_bool().unwrap_or(false);
            let value = Confirm::new(description).with_default(default).prompt()?;
            Ok(serde_json::json!(value))
        }
        _ => {
            if let Some(enum_values) = &opt.enum_values {
                // Strict enum: must pick from list
                let default_str = opt.default.as_str().unwrap_or("");
                let default_idx = enum_values
                    .iter()
                    .position(|v| v == default_str)
                    .unwrap_or(0);
                let selection = Select::new(description, enum_values.clone())
                    .with_starting_cursor(default_idx)
                    .prompt()?;
                Ok(serde_json::json!(selection))
            } else if let Some(proposals) = &opt.proposals {
                // Proposals: suggest values but allow custom
                let mut choices = proposals.clone();
                choices.push("(custom)".to_owned());
                let default_str = opt.default.as_str().unwrap_or("");
                let default_idx = choices.iter().position(|v| v == default_str).unwrap_or(0);
                let selection = Select::new(description, choices)
                    .with_starting_cursor(default_idx)
                    .prompt()?;
                if selection == "(custom)" {
                    let custom = Text::new(&format!("{description} (custom value):"))
                        .with_default(default_str)
                        .prompt()?;
                    Ok(serde_json::json!(custom))
                } else {
                    Ok(serde_json::json!(selection))
                }
            } else {
                // Free-form text
                let default_str = opt.default.as_str().unwrap_or("");
                let value = Text::new(description).with_default(default_str).prompt()?;
                Ok(serde_json::json!(value))
            }
        }
    }
}

/// Prompt for feature options from its metadata JSON.
///
/// Reads the `"options"` object from the feature metadata and prompts
/// for each option.
///
/// # Errors
///
/// Returns error if prompts fail or metadata parsing fails.
pub fn prompt_feature_options(
    feature_id: &str,
    meta: &serde_json::Value,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(options_obj) = meta.get("options").and_then(|o| o.as_object()) else {
        return Ok(HashMap::new());
    };

    if options_obj.is_empty() {
        return Ok(HashMap::new());
    }

    eprintln!("  Configure {feature_id} options:");

    let mut resolved = HashMap::new();
    for (key, opt_value) in options_obj {
        let opt: TemplateOption = match serde_json::from_value(opt_value.clone()) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let value = prompt_single_option(key, &opt)?;
        resolved.insert(key.clone(), value);
    }

    Ok(resolved)
}
