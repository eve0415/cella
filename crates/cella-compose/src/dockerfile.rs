//! Dockerfile reading, stage naming, and combined Dockerfile generation
//! for Docker Compose + features support.
//!
//! Matches the devcontainer CLI's combined-Dockerfile approach: the original
//! Dockerfile (or a synthetic one for image-only services) is concatenated
//! with the feature installation layers into a single Dockerfile that is
//! built in one `docker compose build` pass.

use std::fmt::Write;

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
/// Inserts a global-scope `ARG _DEV_CONTAINERS_BASE_IMAGE=<base_image>`
/// after any parser directives but before the first `FROM`, so that the
/// feature layers' `FROM $_DEV_CONTAINERS_BASE_IMAGE` resolves correctly
/// in `docker compose build` (which cannot pass `--build-arg`).
///
/// The `feature_dockerfile` should be the output of
/// `cella_features::dockerfile::generate_dockerfile()` called with the
/// stage name (from [`ensure_stage_named`] or [`synthetic_dockerfile`])
/// as the `base_image` parameter.
pub fn generate_combined_dockerfile(
    original_content: &str,
    feature_dockerfile: &str,
    base_image: &str,
) -> String {
    let directive_end = find_directive_end_offset(original_content);
    let mut global_arg = String::new();
    writeln!(global_arg, "ARG _DEV_CONTAINERS_BASE_IMAGE={base_image}").unwrap();

    let mut combined = String::with_capacity(
        original_content.len() + global_arg.len() + feature_dockerfile.len() + 2,
    );

    // Preserve parser directives at the top
    combined.push_str(&original_content[..directive_end]);
    // Global ARG so FROM $_DEV_CONTAINERS_BASE_IMAGE resolves across stages
    combined.push_str(&global_arg);
    // Rest of original Dockerfile
    combined.push_str(&original_content[directive_end..]);

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

/// Find the byte offset where Docker parser directives end.
///
/// Docker parser directives (`# syntax=`, `# escape=`, `# check=`) must
/// appear at the very top of a Dockerfile before any blank lines, comments,
/// or build instructions. They have the format `# key=value` where key is
/// a single word. As soon as any non-directive line is encountered
/// (including blank lines or regular comments), scanning stops.
///
/// Returns the byte offset immediately after the last directive's trailing
/// newline, or 0 if no directives are found.
fn find_directive_end_offset(content: &str) -> usize {
    let mut offset = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(after_hash) = trimmed.strip_prefix('#') {
            let rest = after_hash.trim_start();
            if let Some(eq_pos) = rest.find('=') {
                let key = &rest[..eq_pos];
                if !key.is_empty() && !key.contains(char::is_whitespace) {
                    offset += line.len();
                    let remaining = &content[offset..];
                    if remaining.starts_with("\r\n") {
                        offset += 2;
                    } else if remaining.starts_with('\n') {
                        offset += 1;
                    }
                    continue;
                }
            }
        }

        break;
    }

    offset
}

/// A parsed FROM line entry.
struct FromEntry {
    /// Index of this FROM line in the original lines vec.
    line_index: usize,
    /// The stage name after AS, if present.
    stage_name: Option<String>,
    /// The source image token (the `<image>` in `FROM <image> [AS <name>]`).
    ///
    /// Can be another stage name from this Dockerfile or an external image
    /// reference (e.g. `node:18`, `$_DEV_CONTAINERS_BASE_IMAGE`).
    source_image: Option<String>,
}

/// Parse all FROM lines from the Dockerfile, extracting stage names.
///
/// Handles:
/// - `FROM image`
/// - `FROM image AS name`
/// - `FROM image as name` (case-insensitive AS)
/// - Lines with comments after (#)
/// - `FROM --platform=linux/amd64 image [AS name]` (Docker flags)
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

        // Parse: FROM [--flag=value...] <image> [AS <name>]
        // Skip the FROM keyword and any --flag=value tokens to find the image
        let parts: Vec<&str> = without_comment.split_whitespace().collect();
        let non_flag_start = parts
            .iter()
            .skip(1) // skip "FROM"
            .position(|p| !p.starts_with("--"))
            .map_or(parts.len(), |pos| pos + 1);

        let source_image = if non_flag_start < parts.len() {
            Some(parts[non_flag_start].to_string())
        } else {
            None
        };

        let stage_name = if non_flag_start < parts.len()
            && non_flag_start + 2 < parts.len()
            && parts[non_flag_start + 1].eq_ignore_ascii_case("AS")
        {
            Some(parts[non_flag_start + 2].to_string())
        } else {
            None
        };

        entries.push(FromEntry {
            line_index: idx,
            stage_name,
            source_image,
        });
    }

    entries
}

/// Statically resolve the effective USER for a Dockerfile target stage.
///
/// Walks the selected stage (by name, or the last `FROM` if `target_stage`
/// is `None`) looking for the last `USER` instruction before the next
/// `FROM`. Strips any `:group` suffix so the result is a bare user name
/// suitable for Docker's `USER` instruction and the UID-remap `sed` pattern.
///
/// If the stage has no `USER`, follows its `FROM <image>` reference: when
/// `<image>` is another named stage in this Dockerfile, walks into that
/// stage; otherwise returns `None` so the caller can fall back to inspecting
/// the external base image.
///
/// Returns `None` when:
/// - No `USER` directive is found (even after walking stages)
/// - The `USER` argument contains `$` (variable substitution — we don't
///   resolve these; caller should fall back to inspecting the built image)
/// - The target stage is not found
/// - A cycle is detected in the stage chain (defense against malformed input)
#[must_use]
pub fn find_user_statement(content: &str, target_stage: Option<&str>) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let from_entries = parse_from_lines(&lines);

    if from_entries.is_empty() {
        return None;
    }

    let start_idx = if let Some(target) = target_stage {
        from_entries
            .iter()
            .position(|e| e.stage_name.as_deref() == Some(target))?
    } else {
        from_entries.len() - 1
    };

    let mut seen = std::collections::HashSet::new();
    let mut current_idx = start_idx;

    loop {
        if !seen.insert(current_idx) {
            return None;
        }

        let entry = &from_entries[current_idx];
        let stage_start = entry.line_index + 1;
        let stage_end = from_entries
            .get(current_idx + 1)
            .map_or(lines.len(), |next| next.line_index);

        if let Some(user) = find_last_user_in_range(&lines, stage_start, stage_end) {
            return parse_user_token(&user);
        }

        let source = entry.source_image.as_deref()?;
        current_idx = from_entries
            .iter()
            .position(|e| e.stage_name.as_deref() == Some(source))?;
    }
}

/// Return the argument of the last `USER` instruction in `lines[start..end]`,
/// or `None` if no `USER` is present in the range.
///
/// Supports line continuations (`\` at end of line) only at the instruction
/// boundary — the argument itself must fit on one line (matching Docker's
/// behavior for `USER`).
fn find_last_user_in_range(lines: &[&str], start: usize, end: usize) -> Option<String> {
    let mut last: Option<String> = None;

    for line in lines.iter().take(end).skip(start) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let without_comment = trimmed
            .find('#')
            .map_or(trimmed, |hash_pos| trimmed[..hash_pos].trim());

        let mut tokens = without_comment.split_whitespace();
        let Some(instr) = tokens.next() else {
            continue;
        };

        if !instr.eq_ignore_ascii_case("USER") {
            continue;
        }

        let arg: String = tokens.collect::<Vec<_>>().join(" ");
        if !arg.is_empty() {
            last = Some(arg);
        }
    }

    last
}

/// Extract the bare username from a `USER` argument.
///
/// - `node` → `Some("node")`
/// - `node:node` → `Some("node")` (strips `:group`)
/// - `$FOO` or anything containing `$` → `None` (unresolved variable)
/// - empty → `None`
fn parse_user_token(arg: &str) -> Option<String> {
    let trimmed = arg.trim();
    if trimmed.is_empty() || trimmed.contains('$') {
        return None;
    }
    let user = trimmed.split(':').next()?.trim();
    if user.is_empty() {
        None
    } else {
        Some(user.to_string())
    }
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
        let combined = generate_combined_dockerfile(original, features, "base");
        // Global ARG should appear before the first FROM
        let arg_pos = combined
            .find("ARG _DEV_CONTAINERS_BASE_IMAGE=base")
            .unwrap();
        let from_pos = combined.find("FROM node:18 AS base").unwrap();
        assert!(arg_pos < from_pos, "global ARG must precede first FROM");
        assert!(
            combined.contains("FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage")
        );
    }

    #[test]
    fn combined_adds_newline_separator() {
        let original = "FROM node:18";
        let features = "FROM node:18 AS target";
        let combined = generate_combined_dockerfile(original, features, "node:18");
        assert!(combined.contains("ARG _DEV_CONTAINERS_BASE_IMAGE=node:18\nFROM node:18"));
        assert!(combined.contains("\n\nFROM node:18 AS target"));
    }

    #[test]
    fn combined_with_syntax_directive() {
        let original = "# syntax=docker/dockerfile:1\nFROM node:18 AS base\nRUN npm install\n";
        let features = "ARG _DEV_CONTAINERS_BASE_IMAGE=base\nFROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage\nUSER root\n";
        let combined = generate_combined_dockerfile(original, features, "base");

        // Parser directive must remain at the very top
        assert!(combined.starts_with("# syntax=docker/dockerfile:1\n"));
        // Global ARG after directive but before first FROM
        let arg_pos = combined
            .find("ARG _DEV_CONTAINERS_BASE_IMAGE=base")
            .unwrap();
        let syntax_pos = combined.find("# syntax=docker/dockerfile:1").unwrap();
        let from_pos = combined.find("FROM node:18 AS base").unwrap();
        assert!(syntax_pos < arg_pos, "directive must precede global ARG");
        assert!(arg_pos < from_pos, "global ARG must precede first FROM");
    }

    #[test]
    fn combined_with_multiple_directives() {
        let original = "# syntax=docker/dockerfile:1\n# escape=\\\nFROM ubuntu:22.04 AS base\nRUN apt-get update\n";
        let features = "ARG _DEV_CONTAINERS_BASE_IMAGE=base\nFROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage\n";
        let combined = generate_combined_dockerfile(original, features, "base");

        assert!(combined.starts_with("# syntax=docker/dockerfile:1\n# escape=\\\n"));
        let arg_pos = combined
            .find("ARG _DEV_CONTAINERS_BASE_IMAGE=base")
            .unwrap();
        let from_pos = combined.find("FROM ubuntu:22.04 AS base").unwrap();
        assert!(arg_pos < from_pos);
    }

    #[test]
    fn combined_no_directives() {
        let original = "FROM alpine:3.18 AS base\nRUN echo hello\n";
        let features = "ARG _DEV_CONTAINERS_BASE_IMAGE=base\nFROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage\n";
        let combined = generate_combined_dockerfile(original, features, "base");

        assert!(
            combined.starts_with("ARG _DEV_CONTAINERS_BASE_IMAGE=base\nFROM alpine:3.18 AS base")
        );
    }

    #[test]
    fn global_arg_always_before_first_from() {
        // Original has ARG before FROM (common pattern)
        let original = "ARG BASE=node:18\nFROM $BASE AS builder\nRUN npm install\n";
        let features = "ARG _DEV_CONTAINERS_BASE_IMAGE=builder\nFROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage\n";
        let combined = generate_combined_dockerfile(original, features, "builder");

        let global_arg_pos = combined
            .find("ARG _DEV_CONTAINERS_BASE_IMAGE=builder")
            .unwrap();
        let first_from_pos = combined.find("FROM").unwrap();
        assert!(
            global_arg_pos < first_from_pos,
            "global ARG must precede first FROM"
        );
    }

    // ---------------------------------------------------------------
    // find_directive_end_offset
    // ---------------------------------------------------------------

    #[test]
    fn directive_offset_no_directives() {
        assert_eq!(find_directive_end_offset("FROM node:18\nRUN echo hi\n"), 0);
    }

    #[test]
    fn directive_offset_empty_content() {
        assert_eq!(find_directive_end_offset(""), 0);
    }

    #[test]
    fn directive_offset_single_syntax() {
        let content = "# syntax=docker/dockerfile:1\nFROM node:18\n";
        assert_eq!(
            find_directive_end_offset(content),
            "# syntax=docker/dockerfile:1\n".len()
        );
    }

    #[test]
    fn directive_offset_multiple_directives() {
        let content = "# syntax=docker/dockerfile:1\n# escape=\\\nFROM node:18\n";
        let expected = "# syntax=docker/dockerfile:1\n# escape=\\\n".len();
        assert_eq!(find_directive_end_offset(content), expected);
    }

    #[test]
    fn directive_offset_comment_stops_scan() {
        let content = "# This is a comment\nFROM node:18\n";
        assert_eq!(find_directive_end_offset(content), 0);
    }

    #[test]
    fn directive_offset_blank_line_stops_scan() {
        let content = "\n# syntax=docker/dockerfile:1\nFROM node:18\n";
        assert_eq!(find_directive_end_offset(content), 0);
    }

    #[test]
    fn directive_offset_only_directives() {
        let content = "# syntax=docker/dockerfile:1\n# check=skip=all\n";
        assert_eq!(find_directive_end_offset(content), content.len());
    }

    // ---------------------------------------------------------------
    // find_user_statement
    // ---------------------------------------------------------------

    #[test]
    fn user_statement_last_user_wins() {
        let dockerfile = "\
FROM ubuntu:22.04
USER root
RUN apt-get update
USER node
";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("node".to_string())
        );
    }

    #[test]
    fn user_statement_strips_group() {
        let dockerfile = "FROM node:18\nUSER node:node\n";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("node".to_string())
        );
    }

    #[test]
    fn user_statement_strips_numeric_group() {
        let dockerfile = "FROM node:18\nUSER 1000:1000\n";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("1000".to_string())
        );
    }

    #[test]
    fn user_statement_walks_from_chain() {
        let dockerfile = "\
FROM ubuntu:22.04 AS base
USER vscode

FROM base AS dev
RUN echo hello
";
        assert_eq!(
            find_user_statement(dockerfile, Some("dev")),
            Some("vscode".to_string())
        );
    }

    #[test]
    fn user_statement_variable_returns_none() {
        let dockerfile = "FROM node:18\nUSER $FOO\n";
        assert_eq!(find_user_statement(dockerfile, None), None);
    }

    #[test]
    fn user_statement_braced_variable_returns_none() {
        let dockerfile = "FROM node:18\nUSER ${FOO}\n";
        assert_eq!(find_user_statement(dockerfile, None), None);
    }

    #[test]
    fn user_statement_target_not_found() {
        let dockerfile = "FROM node:18 AS base\n";
        assert_eq!(find_user_statement(dockerfile, Some("missing")), None);
    }

    #[test]
    fn user_statement_no_user_anywhere() {
        let dockerfile = "FROM node:18\nRUN echo hi\n";
        assert_eq!(find_user_statement(dockerfile, None), None);
    }

    #[test]
    fn user_statement_cycle_returns_none() {
        // Two stages referencing each other via their stage names — malformed
        // but should not panic or loop forever.
        let dockerfile = "\
FROM b AS a
RUN echo a

FROM a AS b
RUN echo b
";
        assert_eq!(find_user_statement(dockerfile, Some("a")), None);
    }

    #[test]
    fn user_statement_case_insensitive_keyword() {
        let dockerfile = "FROM node:18\nuser node\n";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("node".to_string())
        );
    }

    #[test]
    fn user_statement_whitespace_tolerant() {
        let dockerfile = "FROM node:18\n   USER    node   \n";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("node".to_string())
        );
    }

    #[test]
    fn user_statement_ignores_comments() {
        let dockerfile = "\
FROM node:18
# USER root — just a comment
USER node
";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("node".to_string())
        );
    }

    #[test]
    fn user_statement_strips_inline_comment() {
        let dockerfile = "FROM node:18\nUSER node # runtime user\n";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("node".to_string())
        );
    }

    #[test]
    fn user_statement_stops_at_next_from() {
        // USER in the second stage must not leak into the first stage's result.
        let dockerfile = "\
FROM ubuntu:22.04 AS first
RUN echo first

FROM alpine:3.18 AS second
USER nobody
";
        assert_eq!(find_user_statement(dockerfile, Some("first")), None);
        assert_eq!(
            find_user_statement(dockerfile, Some("second")),
            Some("nobody".to_string())
        );
    }

    #[test]
    fn user_statement_defaults_to_last_stage_when_no_target() {
        let dockerfile = "\
FROM ubuntu:22.04 AS builder
USER root

FROM alpine:3.18
USER user
";
        assert_eq!(
            find_user_statement(dockerfile, None),
            Some("user".to_string())
        );
    }

    #[test]
    fn user_statement_no_from_lines() {
        assert_eq!(find_user_statement("# just a comment\n", None), None);
    }

    #[test]
    fn user_statement_walks_external_base_returns_none() {
        // When a stage has no USER and its FROM is an external image (not a
        // named stage), the caller is expected to fall back to image inspect.
        let dockerfile = "FROM ubuntu:22.04 AS base\nRUN echo hi\n";
        assert_eq!(find_user_statement(dockerfile, Some("base")), None);
    }
}
