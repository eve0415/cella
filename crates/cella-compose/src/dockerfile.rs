//! Dockerfile reading, stage naming, and combined Dockerfile generation
//! for Docker Compose + features support.
//!
//! Matches the devcontainer CLI's combined-Dockerfile approach: the original
//! Dockerfile (or a synthetic one for image-only services) is concatenated
//! with the feature installation layers into a single Dockerfile that is
//! built in one `docker compose build` pass.

use crate::error::CellaComposeError;

/// Default stage name added when a FROM line doesn't have an alias.
pub const AUTO_STAGE_NAME: &str = "dev_container_auto_added_stage_label";

/// The target stage name used by the feature layers.
pub const FEATURES_TARGET_STAGE: &str = "dev_containers_target_stage";

/// Read a Dockerfile and ensure the target stage (or last FROM) has a name.
///
/// If `target` is specified, finds the `FROM ... AS <target>` line and returns
/// it as-is. If no target, finds the last `FROM` line. If the selected FROM
/// already has `AS <name>`, returns the existing name. Otherwise, appends
/// ` AS dev_container_auto_added_stage_label`.
///
/// Returns `(modified_dockerfile_content, stage_name)`.
///
/// # Errors
///
/// Returns an error if the Dockerfile has no FROM instructions, or if the
/// specified target stage is not found.
pub fn ensure_stage_named(
    dockerfile_content: &str,
    target: Option<&str>,
) -> Result<(String, String), CellaComposeError> {
    let lines: Vec<&str> = dockerfile_content.lines().collect();

    // Find all FROM lines with their indices and parsed stage names
    let from_entries: Vec<FromEntry> = parse_from_lines(&lines);

    if from_entries.is_empty() {
        return Err(CellaComposeError::DockerfileParse {
            message: "Dockerfile contains no FROM instructions".to_string(),
        });
    }

    // Find the target FROM entry
    let target_idx = if let Some(target_name) = target {
        // Find the FROM with the matching stage name
        from_entries
            .iter()
            .position(|e| e.stage_name.as_deref() == Some(target_name))
            .ok_or_else(|| CellaComposeError::DockerfileParse {
                message: format!("target stage '{target_name}' not found in Dockerfile"),
            })?
    } else {
        // Use the last FROM
        from_entries.len() - 1
    };

    let entry = &from_entries[target_idx];

    // If already named, return as-is
    if let Some(ref name) = entry.stage_name {
        return Ok((dockerfile_content.to_string(), name.clone()));
    }

    // Append AS <auto_name> to the FROM line
    let mut result_lines: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    let from_line = &result_lines[entry.line_index];
    result_lines[entry.line_index] = format!("{from_line} AS {AUTO_STAGE_NAME}");

    Ok((result_lines.join("\n"), AUTO_STAGE_NAME.to_string()))
}

/// Generate a synthetic Dockerfile for an image-only compose service.
///
/// Returns `(dockerfile_content, stage_name)`.
pub fn synthetic_dockerfile(image: &str) -> (String, String) {
    let content = format!("FROM {image} AS {AUTO_STAGE_NAME}\n");
    (content, AUTO_STAGE_NAME.to_string())
}

/// Generate a combined Dockerfile: original content + feature layers.
///
/// The `feature_dockerfile` should be the output of
/// `cella_features::dockerfile::generate_dockerfile()` called with the
/// stage name (from [`ensure_stage_named`] or [`synthetic_dockerfile`])
/// as the `base_image` parameter.
pub fn generate_combined_dockerfile(original_content: &str, feature_dockerfile: &str) -> String {
    let mut combined = original_content.to_string();
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push('\n');
    combined.push_str(feature_dockerfile);
    combined
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// A parsed FROM line entry.
struct FromEntry {
    /// Index of this FROM line in the original lines vec.
    line_index: usize,
    /// The stage name after AS, if present.
    stage_name: Option<String>,
}

/// Parse all FROM lines from the Dockerfile, extracting stage names.
///
/// Handles:
/// - `FROM image`
/// - `FROM image AS name`
/// - `FROM image as name` (case-insensitive AS)
/// - Lines with comments after (#)
/// - ARG lines before FROM (ignored, they're not FROM lines)
/// - Skips comment lines and empty lines
fn parse_from_lines(lines: &[&str]) -> Vec<FromEntry> {
    let mut entries = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Check if this is a FROM line
        if !trimmed.to_ascii_uppercase().starts_with("FROM ") {
            continue;
        }

        // Strip inline comment
        let without_comment = trimmed
            .find('#')
            .map_or(trimmed, |hash_pos| trimmed[..hash_pos].trim());

        // Parse: FROM <image> [AS <name>]
        let parts: Vec<&str> = without_comment.split_whitespace().collect();
        // parts[0] = "FROM", parts[1] = image, parts[2] = "AS"?, parts[3] = name?
        let stage_name = if parts.len() >= 4 && parts[2].eq_ignore_ascii_case("AS") {
            Some(parts[3].to_string())
        } else {
            None
        };

        entries.push(FromEntry {
            line_index: idx,
            stage_name,
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // ensure_stage_named
    // ---------------------------------------------------------------

    #[test]
    fn single_from_no_name() {
        let dockerfile = "FROM node:18\nRUN npm install\n";
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, AUTO_STAGE_NAME);
        assert!(result.contains(&format!("FROM node:18 AS {AUTO_STAGE_NAME}")));
        assert!(result.contains("RUN npm install"));
    }

    #[test]
    fn single_from_already_named() {
        let dockerfile = "FROM node:18 AS builder\nRUN npm install\n";
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, "builder");
        assert_eq!(result, dockerfile);
    }

    #[test]
    fn multi_stage_no_target_names_last() {
        let dockerfile = "\
FROM node:18 AS builder
RUN npm install

FROM nginx:alpine
COPY --from=builder /app /usr/share/nginx/html
";
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, AUTO_STAGE_NAME);
        assert!(result.contains(&format!("FROM nginx:alpine AS {AUTO_STAGE_NAME}")));
        // First stage unchanged
        assert!(result.contains("FROM node:18 AS builder"));
    }

    #[test]
    fn multi_stage_with_target() {
        let dockerfile = "\
FROM node:18 AS builder
RUN npm install

FROM node:18-slim AS runner
COPY --from=builder /app /app
";
        let (result, name) = ensure_stage_named(dockerfile, Some("runner")).unwrap();
        assert_eq!(name, "runner");
        // Already named, no modification
        assert_eq!(result, dockerfile);
    }

    #[test]
    fn multi_stage_target_without_name() {
        // Target stage exists but isn't named yet — this is unusual but possible
        // if someone specifies target by index. In practice, compose config
        // resolves targets by name, so this is edge-case handling.
        let dockerfile = "\
FROM node:18 AS builder
RUN npm install

FROM node:18-slim
COPY --from=builder /app /app
";
        // No target specified, names the last FROM
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, AUTO_STAGE_NAME);
        assert!(result.contains(&format!("FROM node:18-slim AS {AUTO_STAGE_NAME}")));
    }

    #[test]
    fn target_not_found() {
        let dockerfile = "FROM node:18 AS builder\n";
        let err = ensure_stage_named(dockerfile, Some("nonexistent")).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn no_from_lines() {
        let dockerfile = "# Just a comment\nARG VERSION=1\n";
        let err = ensure_stage_named(dockerfile, None).unwrap_err();
        assert!(err.to_string().contains("no FROM"));
    }

    #[test]
    fn from_with_arg_before() {
        let dockerfile = "\
ARG BASE_IMAGE=node:18
FROM $BASE_IMAGE
RUN echo hello
";
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, AUTO_STAGE_NAME);
        assert!(result.contains(&format!("FROM $BASE_IMAGE AS {AUTO_STAGE_NAME}")));
    }

    #[test]
    fn case_insensitive_as() {
        let dockerfile = "FROM node:18 as myStage\nRUN echo hello\n";
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, "myStage");
        assert_eq!(result, dockerfile);
    }

    #[test]
    fn from_with_inline_comment() {
        let dockerfile = "FROM node:18 # base image\nRUN echo hello\n";
        let (result, name) = ensure_stage_named(dockerfile, None).unwrap();
        assert_eq!(name, AUTO_STAGE_NAME);
        assert!(result.contains(&format!("FROM node:18 # base image AS {AUTO_STAGE_NAME}")));
    }

    // ---------------------------------------------------------------
    // synthetic_dockerfile
    // ---------------------------------------------------------------

    #[test]
    fn synthetic_from_image() {
        let (content, name) = synthetic_dockerfile("node:18");
        assert_eq!(name, AUTO_STAGE_NAME);
        assert_eq!(content, format!("FROM node:18 AS {AUTO_STAGE_NAME}\n"));
    }

    // ---------------------------------------------------------------
    // generate_combined_dockerfile
    // ---------------------------------------------------------------

    #[test]
    fn combined_dockerfile() {
        let original = "FROM node:18 AS base\nRUN npm install\n";
        let features = "ARG _DEV_CONTAINERS_BASE_IMAGE=base\nFROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage\nUSER root\n";
        let combined = generate_combined_dockerfile(original, features);
        assert!(combined.starts_with("FROM node:18 AS base"));
        assert!(combined.contains("ARG _DEV_CONTAINERS_BASE_IMAGE=base"));
        assert!(
            combined.contains("FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage")
        );
    }

    #[test]
    fn combined_adds_newline_separator() {
        let original = "FROM node:18";
        let features = "FROM node:18 AS target";
        let combined = generate_combined_dockerfile(original, features);
        // Should have a blank line between original and features
        assert!(combined.contains("FROM node:18\n\nFROM node:18 AS target"));
    }
}
