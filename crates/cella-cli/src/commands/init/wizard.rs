//! Interactive wizard for `cella init`.
//!
//! Guides the user through template selection, option configuration,
//! feature selection, and config generation using inquire prompts.

use std::collections::HashMap;

use inquire::{Confirm, MultiSelect, Select, Text};

use cella_templates::cache::TemplateCache;
use cella_templates::collection::{self, DEFAULT_FEATURE_COLLECTION, DEFAULT_TEMPLATE_COLLECTION};
use cella_templates::fetcher;
use cella_templates::types::{
    FeatureSummary, OutputFormat, SelectedFeature, TemplateMetadata, TemplateOption,
    TemplateSummary,
};

use super::InitArgs;
use super::summary;
use crate::progress::Progress;

const SHOW_OTHER_REGISTRY: &str = "Show templates from another registry...";
const DONE_ADDING_FEATURES: &str = "Done adding features";

/// Run the interactive init wizard.
///
/// # Errors
///
/// Returns errors for network failures, user cancellation, or I/O errors.
pub async fn run(args: InitArgs, _progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = crate::commands::resolve_workspace_folder(args.workspace_folder.as_deref())?;
    let config_path = workspace.join(".devcontainer").join("devcontainer.json");
    let cache = TemplateCache::new();

    // Step 1: Check for existing config
    if config_path.exists() && !args.force {
        let overwrite = Confirm::new("A devcontainer configuration already exists. Overwrite?")
            .with_default(false)
            .prompt()?;
        if !overwrite {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    // Step 2: Fetch template collection
    let collection_ref = args
        .registry
        .as_deref()
        .unwrap_or(DEFAULT_TEMPLATE_COLLECTION);
    let template_collection =
        collection::fetch_template_collection(collection_ref, &cache, args.refresh).await?;

    // Step 3: Template selection
    let selected_template =
        prompt_template_selection(&template_collection.templates, &cache, args.refresh).await?;

    // Step 4: Fetch full template metadata
    let template_oci_ref = format!(
        "{collection_ref}/{}:{}",
        selected_template.id, selected_template.version
    );
    let template_dir = fetcher::fetch_template(&template_oci_ref, &cache).await?;
    let metadata = fetcher::read_template_metadata(&template_dir)?;

    // Step 5: Template options
    let template_opts = prompt_all_options(&metadata)?;

    // Step 5b: Optional paths
    let excluded_paths = prompt_optional_paths(&metadata)?;

    // Step 6: Feature selection loop
    let features = prompt_feature_loop(&cache, args.refresh).await?;

    // Step 7: Output format
    let format = prompt_output_format()?;

    // Step 8: Summary + confirm
    let opt_display: Vec<(String, String)> = template_opts
        .iter()
        .map(|(k, v)| {
            let display = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), display)
        })
        .collect();

    let template_name = metadata.name.as_deref().unwrap_or(&metadata.id);

    summary::display_summary(template_name, &opt_display, &features, format, &config_path);

    let confirmed = Confirm::new("Write configuration files?")
        .with_default(true)
        .prompt()?;
    if !confirmed {
        eprintln!("Aborted.");
        return Ok(());
    }

    // Step 9: Apply
    let written_path = cella_templates::apply::apply_template(
        &template_dir,
        &workspace,
        &template_opts,
        &features,
        format,
        &excluded_paths,
    )?;

    eprintln!("\u{2713} Created {}", written_path.display());

    // Step 10: Prompt to run cella up
    if args.up {
        eprintln!("Starting dev container...");
        exec_cella_up()?;
    } else {
        let start_now = Confirm::new("Start the dev container now? (cella up)")
            .with_default(false)
            .prompt()?;
        if start_now {
            exec_cella_up()?;
        }
    }

    Ok(())
}

/// Launch `cella up`, replacing the current process on Unix.
fn exec_cella_up() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = std::process::Command::new("cella");
    cmd.arg("up");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec() replaces the process; only returns on failure
        let err = cmd.exec();
        return Err(err.into());
    }

    #[cfg(not(unix))]
    {
        let status = cmd.spawn()?.wait()?;
        if !status.success() {
            return Err(format!("cella up exited with status {status}").into());
        }
        Ok(())
    }
}

/// Prompt the user to select a template from the collection.
async fn prompt_template_selection(
    templates: &[TemplateSummary],
    cache: &TemplateCache,
    refresh: bool,
) -> Result<TemplateSummary, Box<dyn std::error::Error>> {
    loop {
        let mut choices: Vec<String> = templates
            .iter()
            .map(|t| {
                let name = t.name.as_deref().unwrap_or(&t.id);
                let desc = t
                    .description
                    .as_deref()
                    .map(|d| format!(" - {d}"))
                    .unwrap_or_default();
                format!("{name}{desc}")
            })
            .collect();
        choices.push(SHOW_OTHER_REGISTRY.to_owned());

        let selection = Select::new("Select a template:", choices)
            .with_page_size(15)
            .prompt()?;

        if selection == SHOW_OTHER_REGISTRY {
            let registry = Text::new("Enter registry (e.g. ghcr.io/myorg/templates):").prompt()?;
            let custom_collection =
                collection::fetch_template_collection(&registry, cache, refresh).await?;
            if custom_collection.templates.is_empty() {
                eprintln!("No templates found in {registry}.");
                continue;
            }
            // Recurse with custom collection
            return Box::pin(prompt_template_selection(
                &custom_collection.templates,
                cache,
                refresh,
            ))
            .await;
        }

        // Find the selected template by matching the display string
        let idx = templates
            .iter()
            .position(|t| {
                let name = t.name.as_deref().unwrap_or(&t.id);
                selection.starts_with(name)
            })
            .ok_or("template not found in selection")?;

        return Ok(templates[idx].clone());
    }
}

/// Prompt the user for all template options, showing defaults.
fn prompt_all_options(
    metadata: &TemplateMetadata,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error>> {
    if metadata.options.is_empty() {
        return Ok(HashMap::new());
    }

    eprintln!();
    eprintln!("Configure template options:");

    let mut resolved = HashMap::new();
    for (key, opt) in &metadata.options {
        let value = prompt_single_option(key, opt)?;
        resolved.insert(key.clone(), value);
    }

    Ok(resolved)
}

/// Prompt for which optional paths to include via multi-select.
///
/// All optional paths are pre-selected (included by default). Returns the
/// list of paths the user chose to *exclude*.
fn prompt_optional_paths(
    metadata: &TemplateMetadata,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if metadata.optional_paths.is_empty() {
        return Ok(Vec::new());
    }

    let all_indices: Vec<usize> = (0..metadata.optional_paths.len()).collect();
    let included = MultiSelect::new("Include optional paths:", metadata.optional_paths.clone())
        .with_default(&all_indices)
        .prompt()?;

    let excluded: Vec<String> = metadata
        .optional_paths
        .iter()
        .filter(|p| !included.contains(p))
        .cloned()
        .collect();

    Ok(excluded)
}

/// Prompt for a single option value.
fn prompt_single_option(
    key: &str,
    opt: &TemplateOption,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
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

/// Feature selection loop: pick features one at a time, configure each.
async fn prompt_feature_loop(
    cache: &TemplateCache,
    refresh: bool,
) -> Result<Vec<SelectedFeature>, Box<dyn std::error::Error>> {
    // Fetch feature collection
    let feature_collection =
        collection::fetch_feature_collection(DEFAULT_FEATURE_COLLECTION, cache, refresh).await?;
    let mut available: Vec<FeatureSummary> = feature_collection.features;
    let mut selected: Vec<SelectedFeature> = Vec::new();

    loop {
        let mut choices: Vec<String> = vec![DONE_ADDING_FEATURES.to_owned()];
        choices.extend(available.iter().map(|f| {
            let name = f.name.as_deref().unwrap_or(&f.id);
            let desc = f
                .description
                .as_deref()
                .map(|d| format!(" - {d}"))
                .unwrap_or_default();
            format!("{name}{desc}")
        }));

        let selection = Select::new("Add a feature (or Done):", choices)
            .with_page_size(15)
            .prompt()?;

        if selection == DONE_ADDING_FEATURES {
            break;
        }

        // Find selected feature
        let idx = available
            .iter()
            .position(|f| {
                let name = f.name.as_deref().unwrap_or(&f.id);
                selection.starts_with(name)
            })
            .ok_or("feature not found in selection")?;

        let feature_summary = available.remove(idx);

        // Fetch feature metadata to get options
        let feature_ref = format!(
            "{DEFAULT_FEATURE_COLLECTION}/{}:{}",
            feature_summary.id, feature_summary.version
        );

        // Try to fetch and read feature metadata for options
        let feature_options =
            if let Ok(feature_dir) = fetcher::fetch_template(&feature_ref, cache).await {
                // Read devcontainer-feature.json for options
                std::fs::read_to_string(feature_dir.join("devcontainer-feature.json"))
                    .ok()
                    .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
                    .map(|meta| prompt_feature_options(&feature_summary.id, &meta))
                    .transpose()?
                    .unwrap_or_default()
            } else {
                eprintln!("  (could not fetch feature metadata; using defaults)");
                HashMap::new()
            };

        let feature_name = feature_summary
            .name
            .as_deref()
            .unwrap_or(&feature_summary.id);
        eprintln!("\u{2713} Added: {feature_name}");

        selected.push(SelectedFeature {
            reference: feature_ref,
            options: feature_options,
        });
    }

    Ok(selected)
}

/// Prompt for feature options from its metadata JSON.
fn prompt_feature_options(
    feature_id: &str,
    meta: &serde_json::Value,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error>> {
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

/// Prompt for output format.
fn prompt_output_format() -> Result<OutputFormat, Box<dyn std::error::Error>> {
    let choices = vec![
        "JSONC (with comments)".to_owned(),
        "JSON (plain)".to_owned(),
    ];
    let selection = Select::new("Output format:", choices).prompt()?;
    if selection.starts_with("JSON (") {
        Ok(OutputFormat::Json)
    } else {
        Ok(OutputFormat::Jsonc)
    }
}
