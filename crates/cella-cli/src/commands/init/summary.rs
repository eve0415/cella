//! Summary display for the init command confirmation step.

use cella_templates::types::{OutputFormat, SelectedFeature};

/// Display a summary of the init configuration before writing.
#[expect(dead_code, reason = "used by wizard in Phase 5")]
pub fn display_summary(
    template_name: &str,
    template_options: &[(String, String)],
    features: &[SelectedFeature],
    output_format: OutputFormat,
    output_path: &std::path::Path,
) {
    eprintln!();
    eprintln!("Configuration summary:");
    eprintln!("  Template: {template_name}");

    if !template_options.is_empty() {
        for (key, value) in template_options {
            eprintln!("    {key}: {value}");
        }
    }

    if !features.is_empty() {
        eprintln!("  Features:");
        for f in features {
            let short_name = f.reference.rsplit('/').next().unwrap_or(&f.reference);
            if f.options.is_empty() {
                eprintln!("    - {short_name}");
            } else {
                let opts: Vec<String> = f.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
                eprintln!("    - {short_name} ({})", opts.join(", "));
            }
        }
    }

    let format_label = match output_format {
        OutputFormat::Jsonc => "JSONC (with comments)",
        OutputFormat::Json => "JSON",
    };
    eprintln!("  Format:   {format_label}");
    eprintln!("  Output:   {}", output_path.display());
    eprintln!();
}
