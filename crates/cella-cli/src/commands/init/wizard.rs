//! Interactive wizard for `cella init`.
//!
//! Guides the user through template selection, option configuration,
//! feature selection, and config generation using inquire prompts.
//!

use std::collections::HashMap;

use inquire::{Confirm, MultiSelect, Select, Text};
use owo_colors::OwoColorize;

use cella_templates::cache::TemplateCache;
use cella_templates::collection::{self, DEFAULT_FEATURE_COLLECTION, DEFAULT_TEMPLATE_COLLECTION};
use cella_templates::fetcher;
use cella_templates::index::{self, is_official_collection};
use cella_templates::types::{
    DevcontainerIndex, FeatureSummary, IndexCollection, IndexFeatureSummary, IndexTemplateSummary,
    OutputFormat, SelectedFeature, TemplateMetadata, TemplateSummary,
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
pub async fn run(
    args: InitArgs,
    progress: Progress,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    let template_collection = progress
        .run_step_result(
            "Fetching templates",
            collection::fetch_template_collection(collection_ref, &cache, args.refresh),
        )
        .await?;

    // Step 3: Template selection (with multi-source support)
    let (_oci_ref, template_dir, metadata) = select_template(
        &template_collection.templates,
        collection_ref,
        &cache,
        args.refresh,
        &progress,
    )
    .await?;

    // Detect image variant option for pinning support
    let image_variant_info = read_template_config(&template_dir)
        .as_deref()
        .and_then(cella_templates::tags::detect_image_variant_option);

    // Step 4: Template options (with optional pin flow for variant option)
    let (template_opts, pinned_image) = prompt_options_with_pin(
        &metadata,
        image_variant_info.as_ref(),
        &cache,
        args.refresh,
        &progress,
    )
    .await?;

    // Step 4b: Optional paths
    let excluded_paths = prompt_optional_paths(&metadata)?;

    // Step 5: Dev container name
    let container_name = prompt_container_name(&metadata)?;

    // Step 6: Feature selection loop (with multi-source support)
    let features = select_features(&cache, args.refresh, &progress).await?;

    // Step 7: Output format
    let format = prompt_output_format()?;

    // Step 8: Summary + confirm
    if !confirm_summary(
        &metadata,
        &container_name,
        pinned_image.as_deref(),
        &template_opts,
        &features,
        format,
        &config_path,
    )? {
        eprintln!("Aborted.");
        return Ok(());
    }

    // Step 9: Apply
    let overrides = cella_templates::apply::ConfigOverrides {
        name: Some(container_name),
        pinned_image,
        excluded_paths,
    };
    let written_path = cella_templates::apply::apply_template(
        &metadata.id,
        &template_dir,
        &workspace,
        &template_opts,
        &features,
        format,
        &overrides,
    )?;

    // Step 9: Verify the generated config is parseable
    super::verify_generated_config(&written_path);

    // Step 10: Success + optional cella up
    print_success_and_prompt_up(&written_path, args.up)?;

    Ok(())
}

/// Print success message and optionally prompt to run `cella up`.
fn print_success_and_prompt_up(
    written_path: &std::path::Path,
    auto_up: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    eprintln!();
    if auto_up {
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
fn exec_cella_up() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
async fn select_template(
    official_templates: &[TemplateSummary],
    collection_ref: &str,
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<ResolvedTemplate, Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let choice = prompt_official_templates(official_templates)?;
        match choice {
            TemplateChoice::Selected(summary) => {
                let name = summary.name.as_deref().unwrap_or(&summary.id);
                let oci_ref = format!("{collection_ref}/{}:{}", summary.id, summary.version);
                let template_dir = progress
                    .run_step_result(
                        &format!("Fetching template {name}"),
                        fetcher::fetch_template(&oci_ref, cache),
                    )
                    .await?;
                let metadata = fetcher::read_template_metadata(&template_dir)?;
                return Ok((oci_ref, template_dir, Box::new(metadata)));
            }
            TemplateChoice::ShowAllSources => {
                if let Some(result) = browse_all_template_sources(cache, refresh, progress).await? {
                    return Ok(result);
                }
            }
            TemplateChoice::CustomRegistry => {
                if let Some(result) = browse_custom_registry(cache, refresh, progress).await? {
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
) -> Result<TemplateChoice, Box<dyn std::error::Error + Send + Sync>> {
    let mut sorted: Vec<&TemplateSummary> = templates.iter().collect();
    sort_by_display_name(&mut sorted, |t| t.name.as_deref(), |t| &t.id);

    let entries = sorted
        .iter()
        .map(|t| format_entry(t.name.as_deref().unwrap_or(&t.id), t.description.as_deref()));
    let (entry_choices, index_map) = build_choices_with_index(entries);

    let mut choices: Vec<String> = Vec::with_capacity(entry_choices.len() + 2);
    choices.push(SHOW_ALL_SOURCES.to_owned());
    choices.extend(entry_choices);
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

    let idx = resolve_selection(&index_map, &selection)?;
    Ok(TemplateChoice::Selected((*sorted[idx]).clone()))
}

/// Browse all template sources via the aggregated index.
///
/// Returns `None` if the user navigates back to the official list.
async fn browse_all_template_sources(
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<Option<ResolvedTemplate>, Box<dyn std::error::Error + Send + Sync>> {
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

        if let Some(result) = prompt_index_templates(&collection, cache, progress).await? {
            return Ok(Some(result));
        }
        // None = Back to collection picker — loop
    }
}

/// Browse a user-entered custom registry.
async fn browse_custom_registry(
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<Option<ResolvedTemplate>, Box<dyn std::error::Error + Send + Sync>> {
    let registry = Text::new("Enter registry (e.g. ghcr.io/myorg/templates):").prompt()?;
    let custom_collection = progress
        .run_step_result(
            &format!("Fetching templates from {registry}"),
            collection::fetch_template_collection(&registry, cache, refresh),
        )
        .await?;
    if custom_collection.templates.is_empty() {
        eprintln!("No templates found in {registry}.");
        return Ok(None);
    }

    let mut sorted: Vec<&TemplateSummary> = custom_collection.templates.iter().collect();
    sort_by_display_name(&mut sorted, |t| t.name.as_deref(), |t| &t.id);

    let entries = sorted
        .iter()
        .map(|t| format_entry(t.name.as_deref().unwrap_or(&t.id), t.description.as_deref()));
    let (entry_choices, index_map) = build_choices_with_index(entries);

    let mut choices: Vec<String> = Vec::with_capacity(entry_choices.len() + 1);
    choices.push(BACK_TO_OFFICIAL.to_owned());
    choices.extend(entry_choices);

    let selection = Select::new("Select a template:", choices)
        .with_page_size(15)
        .prompt()?;

    if selection == BACK_TO_OFFICIAL {
        return Ok(None);
    }

    let idx = resolve_selection(&index_map, &selection)?;

    let t = sorted[idx];
    let name = t.name.as_deref().unwrap_or(&t.id);
    let oci_ref = format!("{registry}/{}:{}", t.id, t.version);
    let template_dir = progress
        .run_step_result(
            &format!("Fetching template {name}"),
            fetcher::fetch_template(&oci_ref, cache),
        )
        .await?;
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
) -> Result<CollectionPick, Box<dyn std::error::Error + Send + Sync>> {
    let back_label = match kind {
        CollectionKind::Templates => BACK_TO_OFFICIAL,
        CollectionKind::Features => BACK_TO_OFFICIAL_FEATURES,
    };

    let mut relevant: Vec<&IndexCollection> = index
        .collections
        .iter()
        .filter(|c| match kind {
            CollectionKind::Templates => !c.templates.is_empty(),
            CollectionKind::Features => !c.features.is_empty(),
        })
        .collect();
    sort_by_display_name(
        &mut relevant,
        |c| c.source_information.name.as_deref(),
        |c| c.source_information.oci_reference.as_deref().unwrap_or(""),
    );

    let kind_label = match kind {
        CollectionKind::Templates => "templates",
        CollectionKind::Features => "features",
    };

    let entries = relevant.iter().map(|c| {
        let oci_ref = c.source_information.oci_reference.as_deref().unwrap_or("");
        let name = c.source_information.name.as_deref().unwrap_or(oci_ref);
        let count = match kind {
            CollectionKind::Templates => c.templates.len(),
            CollectionKind::Features => c.features.len(),
        };

        if is_official_collection(oci_ref) {
            format!(
                "{} {} {}  {}",
                "\u{2713}".green(),
                name.bold(),
                "(official)".green(),
                format!("{count} {kind_label}").dimmed(),
            )
        } else {
            format!(
                "  {} {}",
                name.bold(),
                format!("{count} {kind_label}").dimmed(),
            )
        }
    });
    let (entry_choices, index_map) = build_choices_with_index(entries);

    let mut choices: Vec<String> = Vec::with_capacity(entry_choices.len() + 1);
    choices.push(back_label.to_owned());
    choices.extend(entry_choices);

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

    let idx = resolve_selection(&index_map, &selection)?;
    Ok(CollectionPick::Selected((*relevant[idx]).clone()))
}

// ── Index template selection ─────────────────────────────────────────

/// Show templates from a specific collection in the aggregated index.
/// Returns `None` if user selects Back.
async fn prompt_index_templates(
    collection: &IndexCollection,
    cache: &TemplateCache,
    progress: &Progress,
) -> Result<Option<ResolvedTemplate>, Box<dyn std::error::Error + Send + Sync>> {
    let source_name = collection
        .source_information
        .name
        .as_deref()
        .unwrap_or("unknown");

    let mut sorted: Vec<&IndexTemplateSummary> = collection.templates.iter().collect();
    sort_by_display_name(&mut sorted, |t| t.name.as_deref(), |t| &t.id);

    let entries = sorted
        .iter()
        .map(|t| format_entry(t.name.as_deref().unwrap_or(&t.id), t.description.as_deref()));
    let (entry_choices, index_map) = build_choices_with_index(entries);

    let mut choices: Vec<String> = Vec::with_capacity(entry_choices.len() + 1);
    choices.push(BACK_TO_SOURCE_LIST.to_owned());
    choices.extend(entry_choices);

    let selection = Select::new(&format!("Select a template (from {source_name}):"), choices)
        .with_page_size(15)
        .prompt()?;

    if selection == BACK_TO_SOURCE_LIST {
        return Ok(None);
    }

    let idx = resolve_selection(&index_map, &selection)?;

    let t = sorted[idx];
    let name = t.name.as_deref().unwrap_or(&t.id);
    let oci_ref = format!("{}:{}", t.id, t.version);
    let template_dir = progress
        .run_step_result(
            &format!("Fetching template {name}"),
            fetcher::fetch_template(&oci_ref, cache),
        )
        .await?;
    let metadata = fetcher::read_template_metadata(&template_dir)?;
    Ok(Some((oci_ref, template_dir, Box::new(metadata))))
}

// ═══════════════════════════════════════════════════════════════════════
// Feature selection (multi-source with back navigation)
// ═══════════════════════════════════════════════════════════════════════

/// Feature selection loop with multi-source support.
async fn select_features(
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<Vec<SelectedFeature>, Box<dyn std::error::Error + Send + Sync>> {
    let feature_collection = progress
        .run_step_result(
            "Fetching features",
            collection::fetch_feature_collection(DEFAULT_FEATURE_COLLECTION, cache, refresh),
        )
        .await?;
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
                let feature = configure_official_feature(&feature_summary, cache, progress).await?;
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
) -> Result<FeatureChoice, Box<dyn std::error::Error + Send + Sync>> {
    // Build sorted indices so we can map back to original positions.
    let mut sorted_indices: Vec<usize> = (0..available.len()).collect();
    sorted_indices.sort_by(|&a, &b| {
        let a_name = available[a].name.as_deref().unwrap_or(&available[a].id);
        let b_name = available[b].name.as_deref().unwrap_or(&available[b].id);
        a_name.to_lowercase().cmp(&b_name.to_lowercase())
    });

    let entries = sorted_indices.iter().map(|&i| {
        let f = &available[i];
        format_entry(f.name.as_deref().unwrap_or(&f.id), f.description.as_deref())
    });
    let (entry_choices, index_map) = build_choices_with_index(entries);

    let mut choices: Vec<String> = Vec::with_capacity(entry_choices.len() + 2);
    choices.push(DONE_ADDING_FEATURES.to_owned());
    choices.push(SHOW_ALL_FEATURE_SOURCES.to_owned());
    choices.extend(entry_choices);

    let selection = Select::new("Add a feature (or Done):", choices)
        .with_page_size(15)
        .prompt()?;

    if selection == DONE_ADDING_FEATURES {
        return Ok(FeatureChoice::Done);
    }
    if selection == SHOW_ALL_FEATURE_SOURCES {
        return Ok(FeatureChoice::ShowAllSources);
    }

    let sorted_idx = resolve_selection(&index_map, &selection)?;
    Ok(FeatureChoice::Selected(sorted_indices[sorted_idx]))
}

/// Configure an official feature (fetch metadata + prompt options).
async fn configure_official_feature(
    feature_summary: &FeatureSummary,
    cache: &TemplateCache,
    progress: &Progress,
) -> Result<SelectedFeature, Box<dyn std::error::Error + Send + Sync>> {
    let feature_ref = format!(
        "{DEFAULT_FEATURE_COLLECTION}/{}:{}",
        feature_summary.id, feature_summary.version
    );

    let feature_options =
        fetch_and_prompt_feature_options(&feature_ref, &feature_summary.id, cache, progress)
            .await?;

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
) -> Result<Option<SelectedFeature>, Box<dyn std::error::Error + Send + Sync>> {
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

        if let Some(feature) = prompt_index_features(&collection, cache, progress).await? {
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
    progress: &Progress,
) -> Result<Option<SelectedFeature>, Box<dyn std::error::Error + Send + Sync>> {
    let source_name = collection
        .source_information
        .name
        .as_deref()
        .unwrap_or("unknown");

    let mut sorted: Vec<&IndexFeatureSummary> = collection.features.iter().collect();
    sort_by_display_name(&mut sorted, |f| f.name.as_deref(), |f| &f.id);

    let entries = sorted
        .iter()
        .map(|f| format_entry(f.name.as_deref().unwrap_or(&f.id), f.description.as_deref()));
    let (entry_choices, index_map) = build_choices_with_index(entries);

    let mut choices: Vec<String> = Vec::with_capacity(entry_choices.len() + 1);
    choices.push(BACK_TO_FEATURE_SOURCE_LIST.to_owned());
    choices.extend(entry_choices);

    let selection = Select::new(&format!("Select a feature (from {source_name}):"), choices)
        .with_page_size(15)
        .prompt()?;

    if selection == BACK_TO_FEATURE_SOURCE_LIST {
        return Ok(None);
    }

    let idx = resolve_selection(&index_map, &selection)?;

    let f = sorted[idx];
    let feature_ref = format!("{}:{}", f.id, f.version);
    let short_id = f.id.rsplit('/').next().unwrap_or(&f.id);
    let feature_options =
        fetch_and_prompt_feature_options(&feature_ref, short_id, cache, progress).await?;

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
    progress: &Progress,
) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    let fetch_result = progress
        .run_step_result(
            &format!("Fetching feature {display_id}"),
            fetcher::fetch_template(feature_ref, cache),
        )
        .await;
    if let Ok(feature_dir) = fetch_result {
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

/// Read the template's `devcontainer.json` content (pre-substitution).
fn read_template_config(template_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(template_dir.join(".devcontainer").join("devcontainer.json"))
        .ok()
        .or_else(|| std::fs::read_to_string(template_dir.join("devcontainer.json")).ok())
}

const PIN_IMAGE_SENTINEL: &str = "Pin to specific image version...";

/// Prompt for template options with optional image pinning support.
///
/// Returns `(options, pinned_image)` where `pinned_image` is `Some` if the
/// user chose to pin to a specific image tag.
async fn prompt_options_with_pin(
    metadata: &TemplateMetadata,
    variant_info: Option<&cella_templates::tags::ImageVariantInfo>,
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<
    (HashMap<String, serde_json::Value>, Option<String>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    if metadata.options.is_empty() {
        return Ok((HashMap::new(), None));
    }

    eprintln!();
    eprintln!("{}", style::label("Configure template options:"));

    let mut resolved = HashMap::new();
    let mut pinned_image: Option<String> = None;

    for (key, opt) in &metadata.options {
        eprintln!();
        eprintln!("  {}", style::label(key));
        if let Some(desc) = &opt.description {
            eprintln!("  {}", style::dim(desc));
        }

        // Check if this is the variant option that supports pinning.
        let is_variant_option =
            variant_info.is_some_and(|info| info.option_key == *key) && opt.proposals.is_some();

        if is_variant_option {
            let (value, pin) =
                prompt_variant_with_pin(key, opt, variant_info.unwrap(), cache, refresh, progress)
                    .await?;
            resolved.insert(key.clone(), value);
            pinned_image = pin;
        } else {
            let value = prompt_single_option(key, opt)?;
            resolved.insert(key.clone(), value);
        }
    }

    Ok((resolved, pinned_image))
}

/// Prompt for an image variant option with a pin sentinel at the end.
///
/// Returns `(chosen_value, optional_pinned_image)`.
async fn prompt_variant_with_pin(
    key: &str,
    opt: &cella_templates::types::TemplateOption,
    variant_info: &cella_templates::tags::ImageVariantInfo,
    cache: &TemplateCache,
    refresh: bool,
    progress: &Progress,
) -> Result<(serde_json::Value, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    let description = opt.description.as_deref().unwrap_or(key);
    let proposals = opt.proposals.as_deref().unwrap_or_default();

    // Build choices: proposals + (custom) + pin sentinel
    let mut choices: Vec<String> = proposals.to_vec();
    choices.push("(custom)".to_owned());
    choices.push(PIN_IMAGE_SENTINEL.to_owned());

    let default_str = opt.default.as_str().unwrap_or("");
    let default_idx = choices.iter().position(|v| v == default_str).unwrap_or(0);

    let selection = Select::new(description, choices)
        .with_starting_cursor(default_idx)
        .prompt()?;

    if selection == "(custom)" {
        let custom = Text::new(&format!("{description} (custom value):"))
            .with_default(default_str)
            .prompt()?;
        return Ok((serde_json::json!(custom), None));
    }

    if selection == PIN_IMAGE_SENTINEL {
        // Step 1: Ask which codename to filter by
        let codename =
            Select::new("Select variant to filter tags:", proposals.to_vec()).prompt()?;

        // Step 2: Fetch tags
        let tags_result = progress
            .run_step_result(
                "Fetching image tags",
                cella_templates::tags::fetch_image_tags(&variant_info.base_image, cache, refresh),
            )
            .await;

        match tags_result {
            Ok(all_tags) => {
                let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
                let mut filtered =
                    cella_templates::tags::filter_tags_by_suffix(&tag_refs, &codename);
                cella_templates::tags::sort_tags_descending(&mut filtered);
                filtered.truncate(cella_templates::tags::MAX_PINNED_TAGS);

                if filtered.is_empty() {
                    eprintln!(
                        "  {} No pinnable tags found for variant \"{codename}\"; using as-is",
                        style::dim("(note)")
                    );
                    return Ok((serde_json::json!(codename), None));
                }

                let pinned_tag = Select::new(
                    "Select image version:",
                    filtered.iter().map(|s| (*s).to_owned()).collect(),
                )
                .with_page_size(15)
                .prompt()?;

                let full_image = format!("{}:{pinned_tag}", variant_info.base_image);
                // Still set the codename as the option value for substitution of
                // other template files that reference this option.
                Ok((serde_json::json!(codename), Some(full_image)))
            }
            Err(e) => {
                eprintln!(
                    "  {} could not fetch image tags: {e}; using variant as-is",
                    style::dim("(note)")
                );
                Ok((serde_json::json!(codename), None))
            }
        }
    } else {
        Ok((serde_json::json!(selection), None))
    }
}

/// Prompt for which optional paths to include via multi-select.
fn prompt_optional_paths(
    metadata: &TemplateMetadata,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
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

/// Prompt for the dev container name.
fn prompt_container_name(
    metadata: &TemplateMetadata,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let default_name = metadata.name.as_deref().unwrap_or(&metadata.id);
    let name = Text::new("Dev container name:")
        .with_default(default_name)
        .prompt()?;
    Ok(name)
}

/// Prompt for output format.
fn prompt_output_format() -> Result<OutputFormat, Box<dyn std::error::Error + Send + Sync>> {
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

/// Display the configuration summary and prompt for confirmation.
fn confirm_summary(
    metadata: &TemplateMetadata,
    container_name: &str,
    pinned_image: Option<&str>,
    template_opts: &HashMap<String, serde_json::Value>,
    features: &[SelectedFeature],
    format: OutputFormat,
    config_path: &std::path::Path,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
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

    summary::display_summary(
        template_name,
        container_name,
        pinned_image,
        &opt_display,
        features,
        format,
        config_path,
    );

    Ok(Confirm::new("Write configuration files?")
        .with_default(true)
        .prompt()?)
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

/// Extract a short display name from a feature OCI reference.
fn feature_display_name(reference: &str) -> String {
    reference
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .to_string()
}

/// Build a choices vec and an index map from formatted entries.
///
/// `inquire::Select` returns the exact string the user picked, so we
/// map each formatted choice back to its original index via exact
/// string equality (not substring matching).
fn build_choices_with_index<I: Iterator<Item = String>>(
    entries: I,
) -> (Vec<String>, HashMap<String, usize>) {
    let mut choices = Vec::new();
    let mut index_map = HashMap::new();
    for (i, entry) in entries.enumerate() {
        index_map.insert(entry.clone(), i);
        choices.push(entry);
    }
    (choices, index_map)
}

/// Look up the original index from a selected choice string.
fn resolve_selection(
    index_map: &HashMap<String, usize>,
    selection: &str,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    index_map
        .get(selection)
        .copied()
        .ok_or_else(|| "item not found in selection".into())
}

/// Sort a slice of items by display name (case-insensitive), falling back to id.
fn sort_by_display_name<T>(
    items: &mut [T],
    get_name: fn(&T) -> Option<&str>,
    get_id: fn(&T) -> &str,
) {
    items.sort_by(|a, b| {
        let a_name = get_name(a).unwrap_or_else(|| get_id(a));
        let b_name = get_name(b).unwrap_or_else(|| get_id(b));
        a_name.to_lowercase().cmp(&b_name.to_lowercase())
    });
}
