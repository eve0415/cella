//! Interactive wizard for `cella init`.
//!
//! Guides the user through template selection, option configuration,
//! feature selection, and config generation using inquire prompts.
//!
//! The wizard runs on a single thread (user interaction), so futures
//! do not need to be `Send`.

use std::collections::HashMap;

use inquire::{Confirm, MultiSelect, Select, Text};
use owo_colors::OwoColorize;

use cella_templates::cache::TemplateCache;
use cella_templates::collection::{self, DEFAULT_FEATURE_COLLECTION, DEFAULT_TEMPLATE_COLLECTION};
use cella_templates::fetcher;
use cella_templates::index::{self, is_official_collection};
use cella_templates::types::{
    DevcontainerIndex, FeatureSummary, IndexCollection, OutputFormat, SelectedFeature,
    TemplateMetadata, TemplateSummary,
};

use crate::commands::features::prompts::{prompt_feature_options, prompt_single_option};
use crate::style;

use super::InitArgs;
use super::summary;
use crate::progress::Progress;

// ── Sentinel labels ──────────────────────────────────────────────────

const SHOW_ALL_SOURCES: &str = "Show all template sources...";
const SHOW_OTHER_REGISTRY: &str = "Show templates from another registry...";
const BACK_TO_OFFICIAL: &str = "\u{2190} Back to official templates";
const BACK_TO_SOURCE_LIST: &str = "\u{2190} Back to source list";

const SHOW_ALL_FEATURE_SOURCES: &str = "Show all feature sources...";
const DONE_ADDING_FEATURES: &str = "Done adding features";
const BACK_TO_OFFICIAL_FEATURES: &str = "\u{2190} Back to official features";
const BACK_TO_FEATURE_SOURCE_LIST: &str = "\u{2190} Back to feature source list";

/// Run the interactive init wizard.
///
/// # Errors
///
/// Returns errors for network failures, user cancellation, or I/O errors.
#[allow(clippy::future_not_send)]
pub async fn run(args: InitArgs, progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
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

    // Step 2: Fetch official template collection
    let collection_ref = args
        .registry
        .as_deref()
        .unwrap_or(DEFAULT_TEMPLATE_COLLECTION);
    let template_collection =
        collection::fetch_template_collection(collection_ref, &cache, args.refresh).await?;

    // Step 3: Template selection (with multi-source support)
    let (_oci_ref, template_dir, metadata) = select_template(
        &template_collection.templates,
        collection_ref,
        &cache,
        args.refresh,
        &progress,
    )
    .await?;

    // Step 4: Template options
    let template_opts = prompt_all_options(&metadata)?;

    // Step 4b: Optional paths
    let excluded_paths = prompt_optional_paths(&metadata)?;

    // Step 5: Feature selection loop (with multi-source support)
    let features = select_features(&cache, args.refresh, &progress).await?;

    // Step 6: Output format
    let format = prompt_output_format()?;

    // Step 7: Summary + confirm
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

    // Step 8: Apply
    let written_path = cella_templates::apply::apply_template(
        &template_dir,
        &workspace,
        &template_opts,
        &features,
        format,
        &excluded_paths,
    )?;

    // Step 9: Success + next steps
    eprintln!();
    eprintln!(
        "{} Created {}",
        style::success_mark(),
        style::value(&written_path.display().to_string())
    );
    eprintln!();
    eprintln!(
        "  {} {}",
        style::hint_arrow(),
        style::dim("Run `cella up` to start the dev container")
    );
    eprintln!(
        "  {} {}",
        style::hint_arrow(),
        style::dim("Edit .devcontainer/devcontainer.json to customize")
    );

    // Step 10: Prompt to run cella up
    eprintln!();
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
#[expect(clippy::needless_return)]
fn exec_cella_up() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = std::process::Command::new("cella");
    cmd.arg("up");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
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

// ═══════════════════════════════════════════════════════════════════════
// Template selection (multi-source with back navigation)
// ═══════════════════════════════════════════════════════════════════════

/// Resolved template: OCI ref, extracted dir, and metadata.
type ResolvedTemplate = (String, std::path::PathBuf, Box<TemplateMetadata>);

/// Select a template through the multi-level navigation flow.
#[allow(clippy::future_not_send)]
async fn select_template(
    official_templates: &[TemplateSummary],
    collection_ref: &str,
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<ResolvedTemplate, Box<dyn std::error::Error>> {
    loop {
        // Bind the choice before any await to avoid holding non-Send
        // Box<dyn Error> across await points.
        let choice = prompt_official_templates(official_templates)?;
        match choice {
            TemplateChoice::Selected(summary) => {
                let oci_ref = format!("{collection_ref}/{}:{}", summary.id, summary.version);
                let template_dir = fetcher::fetch_template(&oci_ref, cache).await?;
                let metadata = fetcher::read_template_metadata(&template_dir)?;
                return Ok((oci_ref, template_dir, Box::new(metadata)));
            }
            TemplateChoice::ShowAllSources => {
                if let Some(result) = browse_all_template_sources(cache, refresh, progress).await? {
                    return Ok(result);
                }
            }
            TemplateChoice::CustomRegistry => {
                if let Some(result) = browse_custom_registry(cache, refresh).await? {
                    return Ok(result);
                }
            }
        }
    }
}

enum TemplateChoice {
    Selected(TemplateSummary),
    ShowAllSources,
    CustomRegistry,
}

/// Show the official template list with sentinel options.
fn prompt_official_templates(
    templates: &[TemplateSummary],
) -> Result<TemplateChoice, Box<dyn std::error::Error>> {
    let mut choices: Vec<String> = Vec::with_capacity(templates.len() + 3);

    choices.push(SHOW_ALL_SOURCES.to_owned());
    choices.push(separator_line());

    for t in templates {
        choices.push(format_entry(
            t.name.as_deref().unwrap_or(&t.id),
            t.description.as_deref(),
        ));
    }

    choices.push(SHOW_OTHER_REGISTRY.to_owned());

    let selection = Select::new("Select a template:", choices)
        .with_page_size(15)
        .prompt()?;

    if selection == SHOW_ALL_SOURCES {
        return Ok(TemplateChoice::ShowAllSources);
    }
    if selection == SHOW_OTHER_REGISTRY {
        return Ok(TemplateChoice::CustomRegistry);
    }
    // Skip separator (shouldn't normally be selectable but guard anyway)
    if selection.contains('─') {
        return prompt_official_templates(templates);
    }

    let idx = find_by_name(
        templates.iter().map(|t| t.name.as_deref().unwrap_or(&t.id)),
        &selection,
    )?;
    Ok(TemplateChoice::Selected(templates[idx].clone()))
}

/// Browse all template sources via the aggregated index.
///
/// Returns `None` if the user navigates back to the official list.
async fn browse_all_template_sources(
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<Option<ResolvedTemplate>, Box<dyn std::error::Error>> {
    let index = progress
        .run_step_result(
            "Fetching template index",
            index::fetch_devcontainer_index(cache, refresh),
        )
        .await?;

    loop {
        let collection = match prompt_collection_picker(&index, CollectionKind::Templates)? {
            CollectionPick::Selected(c) => c,
            CollectionPick::Back => return Ok(None),
        };

        if let Some(result) = prompt_index_templates(&collection, cache).await? {
            return Ok(Some(result));
        }
        // None = Back to collection picker — loop
    }
}

/// Browse a user-entered custom registry.
async fn browse_custom_registry(
    cache: &TemplateCache,
    refresh: bool,
) -> Result<Option<ResolvedTemplate>, Box<dyn std::error::Error>> {
    let registry = Text::new("Enter registry (e.g. ghcr.io/myorg/templates):").prompt()?;
    let custom_collection =
        collection::fetch_template_collection(&registry, cache, refresh).await?;
    if custom_collection.templates.is_empty() {
        eprintln!("No templates found in {registry}.");
        return Ok(None);
    }

    let mut choices: Vec<String> = Vec::with_capacity(custom_collection.templates.len() + 1);
    choices.push(BACK_TO_OFFICIAL.to_owned());
    for t in &custom_collection.templates {
        choices.push(format_entry(
            t.name.as_deref().unwrap_or(&t.id),
            t.description.as_deref(),
        ));
    }

    let selection = Select::new("Select a template:", choices)
        .with_page_size(15)
        .prompt()?;

    if selection == BACK_TO_OFFICIAL {
        return Ok(None);
    }

    let idx = find_by_name(
        custom_collection
            .templates
            .iter()
            .map(|t| t.name.as_deref().unwrap_or(&t.id)),
        &selection,
    )?;

    let t = &custom_collection.templates[idx];
    let oci_ref = format!("{registry}/{}:{}", t.id, t.version);
    let template_dir = fetcher::fetch_template(&oci_ref, cache).await?;
    let metadata = fetcher::read_template_metadata(&template_dir)?;
    Ok(Some((oci_ref, template_dir, Box::new(metadata))))
}

// ── Collection picker ────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum CollectionKind {
    Templates,
    Features,
}

enum CollectionPick {
    Selected(IndexCollection),
    Back,
}

fn prompt_collection_picker(
    index: &DevcontainerIndex,
    kind: CollectionKind,
) -> Result<CollectionPick, Box<dyn std::error::Error>> {
    let back_label = match kind {
        CollectionKind::Templates => BACK_TO_OFFICIAL,
        CollectionKind::Features => BACK_TO_OFFICIAL_FEATURES,
    };

    let relevant: Vec<&IndexCollection> = index
        .collections
        .iter()
        .filter(|c| match kind {
            CollectionKind::Templates => !c.templates.is_empty(),
            CollectionKind::Features => !c.features.is_empty(),
        })
        .collect();

    let mut choices: Vec<String> = Vec::with_capacity(relevant.len() + 1);
    choices.push(back_label.to_owned());

    let kind_label = match kind {
        CollectionKind::Templates => "templates",
        CollectionKind::Features => "features",
    };

    for c in &relevant {
        let oci_ref = c.source_information.oci_reference.as_deref().unwrap_or("");
        let name = c.source_information.name.as_deref().unwrap_or(oci_ref);
        let count = match kind {
            CollectionKind::Templates => c.templates.len(),
            CollectionKind::Features => c.features.len(),
        };

        if is_official_collection(oci_ref) {
            choices.push(format!(
                "{} {} {}  {}",
                "\u{2713}".green(),
                name.bold(),
                "(official)".green(),
                format!("{count} {kind_label}").dimmed(),
            ));
        } else {
            choices.push(format!(
                "  {} {}",
                name.bold(),
                format!("{count} {kind_label}").dimmed(),
            ));
        }
    }

    let prompt_msg = match kind {
        CollectionKind::Templates => "Select a template source:",
        CollectionKind::Features => "Select a feature source:",
    };

    let selection = Select::new(prompt_msg, choices)
        .with_page_size(15)
        .prompt()?;

    if selection == back_label {
        return Ok(CollectionPick::Back);
    }

    let idx = relevant
        .iter()
        .position(|c| {
            let name = c
                .source_information
                .name
                .as_deref()
                .or(c.source_information.oci_reference.as_deref())
                .unwrap_or("");
            selection.contains(name)
        })
        .ok_or("collection not found in selection")?;

    Ok(CollectionPick::Selected((*relevant[idx]).clone()))
}

// ── Index template selection ─────────────────────────────────────────

/// Show templates from a specific collection in the aggregated index.
/// Returns `None` if user selects Back.
async fn prompt_index_templates(
    collection: &IndexCollection,
    cache: &TemplateCache,
) -> Result<Option<ResolvedTemplate>, Box<dyn std::error::Error>> {
    let source_name = collection
        .source_information
        .name
        .as_deref()
        .unwrap_or("unknown");
    let mut choices: Vec<String> = Vec::with_capacity(collection.templates.len() + 1);
    choices.push(BACK_TO_SOURCE_LIST.to_owned());

    for t in &collection.templates {
        choices.push(format_entry(
            t.name.as_deref().unwrap_or(&t.id),
            t.description.as_deref(),
        ));
    }

    let selection = Select::new(&format!("Select a template (from {source_name}):"), choices)
        .with_page_size(15)
        .prompt()?;

    if selection == BACK_TO_SOURCE_LIST {
        return Ok(None);
    }

    let idx = find_by_name(
        collection
            .templates
            .iter()
            .map(|t| t.name.as_deref().unwrap_or(&t.id)),
        &selection,
    )?;

    let t = &collection.templates[idx];
    let oci_ref = format!("{}:{}", t.id, t.version);
    let template_dir = fetcher::fetch_template(&oci_ref, cache).await?;
    let metadata = fetcher::read_template_metadata(&template_dir)?;
    Ok(Some((oci_ref, template_dir, Box::new(metadata))))
}

// ═══════════════════════════════════════════════════════════════════════
// Feature selection (multi-source with back navigation)
// ═══════════════════════════════════════════════════════════════════════

/// Feature selection loop with multi-source support.
#[allow(clippy::future_not_send)]
async fn select_features(
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<Vec<SelectedFeature>, Box<dyn std::error::Error>> {
    let feature_collection =
        collection::fetch_feature_collection(DEFAULT_FEATURE_COLLECTION, cache, refresh).await?;
    let mut available: Vec<FeatureSummary> = feature_collection.features;
    let mut selected: Vec<SelectedFeature> = Vec::new();

    loop {
        let choice = prompt_feature_list(&available)?;
        match choice {
            FeatureChoice::Done => break,
            FeatureChoice::ShowAllSources => {
                if let Some(feature) = browse_all_feature_sources(cache, refresh, progress).await? {
                    let feature_name = feature_display_name(&feature.reference);
                    eprintln!(
                        "{} Added: {}",
                        style::success_mark(),
                        style::value(&feature_name)
                    );
                    selected.push(feature);
                }
            }
            FeatureChoice::Selected(idx) => {
                let feature_summary = available.remove(idx);
                let feature = configure_official_feature(&feature_summary, cache).await?;
                selected.push(feature);
            }
        }
    }

    Ok(selected)
}

enum FeatureChoice {
    Done,
    ShowAllSources,
    Selected(usize),
}

fn prompt_feature_list(
    available: &[FeatureSummary],
) -> Result<FeatureChoice, Box<dyn std::error::Error>> {
    let mut choices: Vec<String> = Vec::with_capacity(available.len() + 2);
    choices.push(DONE_ADDING_FEATURES.to_owned());
    choices.push(SHOW_ALL_FEATURE_SOURCES.to_owned());

    for f in available {
        choices.push(format_entry(
            f.name.as_deref().unwrap_or(&f.id),
            f.description.as_deref(),
        ));
    }

    let selection = Select::new("Add a feature (or Done):", choices)
        .with_page_size(15)
        .prompt()?;

    if selection == DONE_ADDING_FEATURES {
        return Ok(FeatureChoice::Done);
    }
    if selection == SHOW_ALL_FEATURE_SOURCES {
        return Ok(FeatureChoice::ShowAllSources);
    }

    let idx = find_by_name(
        available.iter().map(|f| f.name.as_deref().unwrap_or(&f.id)),
        &selection,
    )?;

    Ok(FeatureChoice::Selected(idx))
}

/// Configure an official feature (fetch metadata + prompt options).
async fn configure_official_feature(
    feature_summary: &FeatureSummary,
    cache: &TemplateCache,
) -> Result<SelectedFeature, Box<dyn std::error::Error>> {
    let feature_ref = format!(
        "{DEFAULT_FEATURE_COLLECTION}/{}:{}",
        feature_summary.id, feature_summary.version
    );

    let feature_options =
        fetch_and_prompt_feature_options(&feature_ref, &feature_summary.id, cache).await?;

    let feature_name = feature_summary
        .name
        .as_deref()
        .unwrap_or(&feature_summary.id);
    eprintln!(
        "{} Added: {}",
        style::success_mark(),
        style::value(feature_name)
    );

    Ok(SelectedFeature {
        reference: feature_ref,
        options: feature_options,
    })
}

/// Browse all feature sources via the aggregated index.
async fn browse_all_feature_sources(
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<Option<SelectedFeature>, Box<dyn std::error::Error>> {
    let index = progress
        .run_step_result(
            "Fetching feature index",
            index::fetch_devcontainer_index(cache, refresh),
        )
        .await?;

    loop {
        let collection = match prompt_collection_picker(&index, CollectionKind::Features)? {
            CollectionPick::Selected(c) => c,
            CollectionPick::Back => return Ok(None),
        };

        if let Some(feature) = prompt_index_features(&collection, cache).await? {
            return Ok(Some(feature));
        }
        // None = Back to collection picker — loop
    }
}

/// Show features from a specific collection in the aggregated index.
/// Returns `None` if user selects Back.
async fn prompt_index_features(
    collection: &IndexCollection,
    cache: &TemplateCache,
) -> Result<Option<SelectedFeature>, Box<dyn std::error::Error>> {
    let source_name = collection
        .source_information
        .name
        .as_deref()
        .unwrap_or("unknown");
    let mut choices: Vec<String> = Vec::with_capacity(collection.features.len() + 1);
    choices.push(BACK_TO_FEATURE_SOURCE_LIST.to_owned());

    for f in &collection.features {
        choices.push(format_entry(
            f.name.as_deref().unwrap_or(&f.id),
            f.description.as_deref(),
        ));
    }

    let selection = Select::new(&format!("Select a feature (from {source_name}):"), choices)
        .with_page_size(15)
        .prompt()?;

    if selection == BACK_TO_FEATURE_SOURCE_LIST {
        return Ok(None);
    }

    let idx = find_by_name(
        collection
            .features
            .iter()
            .map(|f| f.name.as_deref().unwrap_or(&f.id)),
        &selection,
    )?;

    let f = &collection.features[idx];
    let feature_ref = format!("{}:{}", f.id, f.version);
    let short_id = f.id.rsplit('/').next().unwrap_or(&f.id);
    let feature_options = fetch_and_prompt_feature_options(&feature_ref, short_id, cache).await?;

    Ok(Some(SelectedFeature {
        reference: feature_ref,
        options: feature_options,
    }))
}

/// Fetch feature metadata and prompt for options.
async fn fetch_and_prompt_feature_options(
    feature_ref: &str,
    display_id: &str,
    cache: &TemplateCache,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error>> {
    if let Ok(feature_dir) = fetcher::fetch_template(feature_ref, cache).await {
        let options = std::fs::read_to_string(feature_dir.join("devcontainer-feature.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .map(|meta| prompt_feature_options(display_id, &meta))
            .transpose()?
            .unwrap_or_default();
        Ok(options)
    } else {
        eprintln!(
            "  {} could not fetch feature metadata; using defaults",
            style::dim("(note)")
        );
        Ok(HashMap::new())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Template options, optional paths, output format
// ═══════════════════════════════════════════════════════════════════════

/// Prompt the user for all template options, showing defaults.
fn prompt_all_options(
    metadata: &TemplateMetadata,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error>> {
    if metadata.options.is_empty() {
        return Ok(HashMap::new());
    }

    eprintln!();
    eprintln!("{}", style::label("Configure template options:"));

    let mut resolved = HashMap::new();
    for (key, opt) in &metadata.options {
        eprintln!();
        eprintln!("  {}", style::label(key));
        if let Some(desc) = &opt.description {
            eprintln!("  {}", style::dim(desc));
        }
        let value = prompt_single_option(key, opt)?;
        resolved.insert(key.clone(), value);
    }

    Ok(resolved)
}

/// Prompt for which optional paths to include via multi-select.
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

// ═══════════════════════════════════════════════════════════════════════
// Formatting helpers
// ═══════════════════════════════════════════════════════════════════════

/// Format a template/feature entry: bold name + dim description.
fn format_entry(name: &str, description: Option<&str>) -> String {
    description.map_or_else(
        || format!("{}", name.bold()),
        |desc| format!("{} {} {}", name.bold(), "-".dimmed(), desc.dimmed()),
    )
}

/// A dim separator line for visual grouping in picker lists.
fn separator_line() -> String {
    format!("{}", "─".repeat(40).dimmed())
}

/// Extract a short display name from a feature OCI reference.
fn feature_display_name(reference: &str) -> String {
    reference
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .to_string()
}

/// Find the index of a selection by matching the name within the display string.
fn find_by_name<'a>(
    names: impl Iterator<Item = &'a str>,
    selection: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    names
        .enumerate()
        .find_map(|(i, name)| selection.contains(name).then_some(i))
        .ok_or_else(|| "item not found in selection".into())
}
