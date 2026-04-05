//! Summary display for the init command confirmation step.

use cella_templates::types::{OutputFormat, SelectedFeature};

use crate::style;

/// Display a summary of the init configuration before writing.
pub fn display_summary(
    template_name: &str,
    container_name: &str,
    template_options: &[(String, String)],
    features: &[SelectedFeature],
    output_format: OutputFormat,
    output_path: &std::path::Path,
) {
    eprintln!();
    eprintln!("{}", style::label("Configuration summary:"));
    eprintln!(
        "  {}  {}",
        style::label("Template:"),
        style::value(template_name)
    );
    eprintln!(
        "  {}      {}",
        style::label("Name:"),
        style::value(container_name)
    );

    if !template_options.is_empty() {
        for (key, value) in template_options {
            eprintln!("    {}: {}", style::dim(key), style::value(value));
        }
    }

    if !features.is_empty() {
        eprintln!("  {}", style::label("Features:"));
        for f in features {
            let short_name = f.reference.rsplit('/').next().unwrap_or(&f.reference);
            if f.options.is_empty() {
                eprintln!("    - {}", style::value(short_name));
            } else {
                let opts: Vec<String> = f.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
                eprintln!(
                    "    - {} {}",
                    style::value(short_name),
                    style::dim(&format!("({})", opts.join(", ")))
                );
            }
        }
    }

    let format_label = match output_format {
        OutputFormat::Jsonc => "JSONC (with comments)",
        OutputFormat::Json => "JSON",
    };
    eprintln!(
        "  {}  {}",
        style::label("Format:"),
        style::value(format_label)
    );
    eprintln!(
        "  {}  {}",
        style::label("Output:"),
        style::value(&output_path.display().to_string())
    );
    eprintln!();
}
