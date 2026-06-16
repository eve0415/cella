//! `cella features info` — inspect OCI feature manifests, tags, and dependencies.
//!
//! Drop-in replacement for `devcontainer features info`.
//! Exact contract:
//!   cella features info <mode> <feature>
//!   --log-level  info|debug|trace  (default info)
//!   --output-format text|json      (default text)

use clap::{Parser, ValueEnum};
use serde::Serialize;

use crate::commands::LogLevel;
use cella_features::graph::{build_dependency_graph, render_mermaid};
use cella_oci::{fetch_manifest_with_digest, fetch_published_tags};

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// Show information about a devcontainer feature from an OCI registry.
#[derive(Debug, Clone, Parser)]
pub struct InfoArgs {
    /// What to show: manifest | tags | dependencies | verbose.
    pub mode: InfoMode,

    /// OCI feature reference, e.g. `ghcr.io/devcontainers/features/node:1`.
    pub feature: String,

    /// Log verbosity.
    #[arg(long, default_value = "info")]
    pub log_level: LogLevel,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output_format: OutputFormat,
}

/// Info sub-mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InfoMode {
    /// Show the OCI manifest JSON.
    Manifest,
    /// List published tags.
    Tags,
    /// Render the dependency graph as a Mermaid diagram.
    Dependencies,
    /// Show manifest + tags + dependency graph.
    Verbose,
}

/// Output format.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

// ---------------------------------------------------------------------------
// JSON output shapes (camelCase to match official CLI)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestOutput {
    manifest: serde_json::Value,
    canonical_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TagsOutput {
    published_tags: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VerboseOutput {
    manifest: serde_json::Value,
    canonical_id: String,
    published_tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

impl InfoArgs {
    /// Execute the info command.
    ///
    /// # Errors
    ///
    /// Returns an error on OCI fetch failure or serialisation errors.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.mode {
            InfoMode::Manifest => self.run_manifest().await,
            InfoMode::Tags => self.run_tags().await,
            InfoMode::Dependencies => self.run_dependencies().await,
            InfoMode::Verbose => self.run_verbose().await,
        }
    }

    async fn run_manifest(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match fetch_manifest_with_digest(&self.feature).await {
            Ok((manifest, digest)) => {
                let canonical_id = build_canonical_id(&self.feature, &digest);
                match self.output_format {
                    OutputFormat::Json => {
                        let out = ManifestOutput {
                            manifest,
                            canonical_id,
                        };
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    }
                    OutputFormat::Text => {
                        println!("{}", enclose_string_in_box("Manifest", 0));
                        println!("{}\n", serde_json::to_string_pretty(&manifest)?);
                        println!("{}", enclose_string_in_box("Canonical Identifier", 0));
                        println!("{canonical_id}\n");
                    }
                }
            }
            Err(e) => {
                return Err(manifest_fetch_error(e, self.output_format));
            }
        }
        Ok(())
    }

    async fn run_tags(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match fetch_published_tags(&self.feature).await {
            Ok(tags) if tags.is_empty() => {
                return Err(tags_empty_error(self.output_format));
            }
            Ok(tags) => match self.output_format {
                OutputFormat::Json => {
                    let out = TagsOutput {
                        published_tags: tags,
                    };
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                OutputFormat::Text => {
                    println!("{}", enclose_string_in_box("Published Tags", 0));
                    println!("{}", tags.join("\n   "));
                }
            },
            Err(e) => {
                return Err(tags_fetch_error(e, self.output_format));
            }
        }
        Ok(())
    }

    async fn run_dependencies(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.output_format {
            OutputFormat::Json => {
                // NOTE: the official devcontainer CLI outputs nothing in JSON mode for
                // the `dependencies` sub-mode. This is a known omission in the upstream
                // implementation. We match that behaviour for parity.
            }
            OutputFormat::Text => {
                let graph = build_dependency_graph(&[self.feature.as_str()]).await?;
                let diagram = render_mermaid(&[self.feature.as_str()], &graph.edges);
                println!(
                    "{}",
                    enclose_string_in_box("Dependency Tree (Render with https://mermaid.live/)", 0)
                );
                println!("{diagram}");
            }
        }
        Ok(())
    }

    async fn run_verbose(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let manifest_result = fetch_manifest_with_digest(&self.feature).await;
        let tags_result = fetch_published_tags(&self.feature).await;

        match self.output_format {
            OutputFormat::Json => {
                let (manifest, canonical_id) = match manifest_result {
                    Ok((m, d)) => (m, build_canonical_id(&self.feature, &d)),
                    Err(e) => return Err(manifest_fetch_error(e, self.output_format)),
                };
                let published_tags =
                    tags_result.map_err(|e| tags_fetch_error(e, self.output_format))?;
                let out = VerboseOutput {
                    manifest,
                    canonical_id,
                    published_tags,
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            OutputFormat::Text => {
                // Manifest section
                match manifest_result {
                    Ok((manifest, digest)) => {
                        let canonical_id = build_canonical_id(&self.feature, &digest);
                        println!("{}", enclose_string_in_box("Manifest", 0));
                        println!("{}\n", serde_json::to_string_pretty(&manifest)?);
                        println!("{}", enclose_string_in_box("Canonical Identifier", 0));
                        println!("{canonical_id}\n");
                    }
                    Err(e) => {
                        eprintln!("Failed to fetch manifest: {e}");
                    }
                }
                // Tags section
                match tags_result {
                    Ok(tags) => {
                        println!("{}", enclose_string_in_box("Published Tags", 0));
                        println!("{}", tags.join("\n   "));
                    }
                    Err(e) => {
                        eprintln!("Failed to fetch tags: {e}");
                    }
                }
                // Dependencies section — failure is non-fatal, symmetric with
                // the manifest and tags sections above.
                match build_dependency_graph(&[self.feature.as_str()]).await {
                    Ok(graph) => {
                        println!(
                            "{}",
                            enclose_string_in_box(
                                "Dependency Tree (Render with https://mermaid.live/)",
                                0
                            )
                        );
                        println!("{}", render_mermaid(&[self.feature.as_str()], &graph.edges));
                    }
                    Err(e) => {
                        eprintln!("Failed to build dependency graph: {e}");
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Build the canonical OCI identifier: `registry/repository@sha256:hex`.
///
/// Handles three reference forms correctly:
/// - `registry/repo/name:tag`  → strips `:tag`, appends digest
/// - `registry/repo/name@sha256:<hex>` → strips `@…` suffix, appends digest
/// - `localhost:5000/ns/name`  → no tag after last `/`, appends digest as-is
///
/// The `:` in a registry host (`localhost:5000`) is distinguished from a tag
/// separator by requiring the `:` to appear *after* the last `/`.
pub fn build_canonical_id(reference: &str, hex_digest: &str) -> String {
    // Strip an existing digest reference (@sha256:...) first.
    let base = reference.find('@').map_or_else(
        || {
            // Look for a tag-separating `:` — it must appear after the last `/`.
            let last_slash = reference.rfind('/').map_or(0, |p| p + 1);
            reference[last_slash..]
                .find(':')
                .map_or(reference, |rel| &reference[..last_slash + rel])
        },
        |pos| &reference[..pos],
    );
    format!("{base}@sha256:{hex_digest}")
}

/// Construct the boxed error returned on manifest fetch failure.
fn manifest_fetch_error(
    e: impl std::fmt::Display,
    fmt: OutputFormat,
) -> Box<dyn std::error::Error + Send + Sync> {
    match fmt {
        OutputFormat::Json => {
            println!("{{}}");
            format!("failed to fetch manifest: {e}").into()
        }
        OutputFormat::Text => format!("failed to fetch manifest: {e}").into(),
    }
}

/// Construct the boxed error returned when no tags are found.
fn tags_empty_error(fmt: OutputFormat) -> Box<dyn std::error::Error + Send + Sync> {
    match fmt {
        OutputFormat::Json => {
            println!("{{}}");
            "no published tags found".into()
        }
        OutputFormat::Text => "no published tags found".into(),
    }
}

/// Construct the boxed error returned on tag fetch failure.
fn tags_fetch_error(
    e: impl std::fmt::Display,
    fmt: OutputFormat,
) -> Box<dyn std::error::Error + Send + Sync> {
    match fmt {
        OutputFormat::Json => {
            println!("{{}}");
            format!("failed to fetch tags: {e}").into()
        }
        OutputFormat::Text => format!("failed to fetch tags: {e}").into(),
    }
}

// ---------------------------------------------------------------------------
// Box drawing helper
// ---------------------------------------------------------------------------

/// Enclose a string in a Unicode box, matching the official devcontainer CLI
/// `encloseStringInBox` helper exactly.
///
/// Single-line and multi-line inputs are supported.  The first line is wrapped
/// in ANSI bold (`\x1b[1m` … `\x1b[22m`) unconditionally — no TTY check —
/// to match the official behaviour.  The box width equals the maximum VISIBLE
/// character width across all lines: only the bold markers this helper injects
/// on the first line are discounted (the official likewise compensates solely
/// for those, not for arbitrary ANSI in the input).
///
/// `indent` prepends that many spaces to every line of output.
fn enclose_string_in_box(s: &str, indent: usize) -> String {
    // The ANSI bold markers add 9 bytes (4 + 5) but zero visible columns.
    const ANSI_BOLD_BYTES: usize = 9;

    let raw_lines: Vec<&str> = s.split('\n').collect();

    // Build the display lines: first line gets bold markers injected.
    let display_lines: Vec<String> = raw_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("\x1b[1m{line}\x1b[22m")
            } else {
                (*line).to_owned()
            }
        })
        .collect();

    // Visible width per line: subtract ANSI bytes for the line that carries them.
    let max_width = display_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let char_count = line.chars().count();
            if i == 0 {
                char_count.saturating_sub(ANSI_BOLD_BYTES)
            } else {
                char_count
            }
        })
        .max()
        .unwrap_or(0);

    let pad = " ".repeat(indent);
    let top = format!("{pad}┌{}┐", "─".repeat(max_width));
    let bottom = format!("{pad}└{}┘", "─".repeat(max_width));

    let middle: Vec<String> = display_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            // How many spaces are needed to reach max_width visible columns?
            let visible_len = if i == 0 {
                line.chars().count().saturating_sub(ANSI_BOLD_BYTES)
            } else {
                line.chars().count()
            };
            let padding = " ".repeat(max_width.saturating_sub(visible_len));
            format!("{pad}│{line}{padding}│")
        })
        .collect();

    let mut parts = Vec::with_capacity(2 + middle.len());
    parts.push(top);
    parts.extend(middle);
    parts.push(bottom);
    parts.join("\n")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // enclose_string_in_box
    // -----------------------------------------------------------------------

    #[test]
    fn box_manifest_exact_bytes() {
        // Must match official encloseStringInBox("Manifest", 0) byte-for-byte.
        // "Manifest" is 8 visible chars → 8 ─ chars in top/bottom borders.
        // The content line: │ + \x1b[1m + Manifest + \x1b[22m + │
        let result = enclose_string_in_box("Manifest", 0);
        let expected = "┌────────┐\n│\x1b[1mManifest\x1b[22m│\n└────────┘";
        assert_eq!(result, expected);
    }

    #[test]
    fn box_single_line_no_indent() {
        // "AB" = 2 visible chars → 2 dashes, no trailing padding needed.
        let result = enclose_string_in_box("AB", 0);
        assert_eq!(result, "┌──┐\n│\x1b[1mAB\x1b[22m│\n└──┘");
    }

    #[test]
    fn box_with_indent() {
        let result = enclose_string_in_box("Hi", 2);
        assert_eq!(result, "  ┌──┐\n  │\x1b[1mHi\x1b[22m│\n  └──┘");
    }

    #[test]
    fn box_canonical_identifier_width() {
        // "Canonical Identifier" is 20 chars → 20 dashes.
        let result = enclose_string_in_box("Canonical Identifier", 0);
        assert!(result.starts_with("┌────────────────────┐"));
    }

    #[test]
    fn box_dependency_tree_header() {
        let header = "Dependency Tree (Render with https://mermaid.live/)";
        let result = enclose_string_in_box(header, 0);
        // 51 visible chars → 51 dashes (count chars, mirroring the impl's width).
        let dash_line = "─".repeat(header.chars().count());
        assert!(result.starts_with(&format!("┌{dash_line}┐")));
    }

    // -----------------------------------------------------------------------
    // InfoMode parsing via clap ValueEnum
    // -----------------------------------------------------------------------

    #[test]
    fn info_mode_from_str_all_variants() {
        use clap::ValueEnum;
        assert!(InfoMode::from_str("manifest", true).is_ok());
        assert!(InfoMode::from_str("tags", true).is_ok());
        assert!(InfoMode::from_str("dependencies", true).is_ok());
        assert!(InfoMode::from_str("verbose", true).is_ok());
    }

    #[test]
    fn info_mode_invalid_rejected() {
        use clap::ValueEnum;
        assert!(InfoMode::from_str("unknown-mode", true).is_err());
        assert!(InfoMode::from_str("", true).is_err());
    }

    // -----------------------------------------------------------------------
    // JSON serialisation shape
    // -----------------------------------------------------------------------

    #[test]
    fn manifest_output_camel_case_keys() {
        let out = ManifestOutput {
            manifest: serde_json::json!({"schemaVersion": 2}),
            canonical_id: "ghcr.io/devcontainers/features/node@sha256:abc".to_owned(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert!(json.get("manifest").is_some(), "manifest key missing");
        assert!(json.get("canonicalId").is_some(), "canonicalId key missing");
        assert!(
            json.get("canonical_id").is_none(),
            "snake_case key must not appear"
        );
    }

    #[test]
    fn tags_output_camel_case_keys() {
        let out = TagsOutput {
            published_tags: vec!["1".to_owned(), "latest".to_owned()],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert!(
            json.get("publishedTags").is_some(),
            "publishedTags key missing"
        );
        assert!(
            json.get("published_tags").is_none(),
            "snake_case key must not appear"
        );
    }

    #[test]
    fn verbose_output_camel_case_keys() {
        let out = VerboseOutput {
            manifest: serde_json::json!({}),
            canonical_id: "id".to_owned(),
            published_tags: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert!(json.get("manifest").is_some());
        assert!(json.get("canonicalId").is_some());
        assert!(json.get("publishedTags").is_some());
    }

    // -----------------------------------------------------------------------
    // build_canonical_id
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_id_plain_ref_no_tag() {
        // No tag, no digest — just append.
        let id = build_canonical_id("ghcr.io/devcontainers/features/node", "abc123");
        assert_eq!(id, "ghcr.io/devcontainers/features/node@sha256:abc123");
    }

    #[test]
    fn canonical_id_strips_tag_appends_digest() {
        let id = build_canonical_id("ghcr.io/devcontainers/features/node:1", "abc123");
        assert_eq!(id, "ghcr.io/devcontainers/features/node@sha256:abc123");
    }

    #[test]
    fn canonical_id_digest_ref_does_not_double_append() {
        // Input already has @sha256:... — strip it and use our digest.
        let id = build_canonical_id(
            "ghcr.io/devcontainers/features/node@sha256:deadbeef",
            "abc123",
        );
        assert_eq!(id, "ghcr.io/devcontainers/features/node@sha256:abc123");
    }

    #[test]
    fn canonical_id_registry_with_port_no_tag() {
        // The `:5000` is part of the registry host, not a tag separator.
        let id = build_canonical_id("localhost:5000/ns/myfeature", "abc123");
        assert_eq!(id, "localhost:5000/ns/myfeature@sha256:abc123");
    }

    #[test]
    fn canonical_id_registry_with_port_and_tag() {
        let id = build_canonical_id("localhost:5000/ns/myfeature:latest", "abc123");
        assert_eq!(id, "localhost:5000/ns/myfeature@sha256:abc123");
    }
}
