//! Image tag fetching, filtering, and sorting for version pinning.
//!
//! Enables users to pin a devcontainer to a specific image version
//! (e.g. `4.0.6-22-trixie`) rather than using the template's default
//! tag pattern.

use tracing::debug;

/// Information about the image variant option detected in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageVariantInfo {
    /// Base image without tag (e.g. `mcr.microsoft.com/devcontainers/rust`).
    pub base_image: String,
    /// Template option key that controls the variant (e.g. `imageVariant`).
    pub option_key: String,
}

/// Detect which template option controls the image variant by parsing the
/// template's `devcontainer.json` content (JSONC accepted — comments and
/// trailing commas are stripped before parsing).
///
/// Looks for `${templateOption:KEY}` in the `"image"` field value. Returns
/// the last option reference found in the tag portion (after `:`).
pub fn detect_image_variant_option(config_content: &str) -> Option<ImageVariantInfo> {
    let stripped = match cella_jsonc::strip(config_content) {
        Ok(s) => s,
        Err(e) => {
            debug!("variant detection: stripping JSONC failed: {e}");
            return None;
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(e) => {
            debug!("variant detection: parsing template config failed: {e}");
            return None;
        }
    };
    let image = parsed.get("image")?.as_str()?;

    // Split image into base and tag. The tag separator is the first ':'
    // that appears after the last '/' (to avoid matching colons inside
    // `${templateOption:...}` or port numbers).
    let last_slash = image.rfind('/').unwrap_or(0);
    let (base, tag_portion) = image[last_slash..]
        .find(':')
        .map_or((image, ""), |colon_offset| {
            let colon_pos = last_slash + colon_offset;
            (&image[..colon_pos], &image[colon_pos + 1..])
        });

    // A placeholder in the base (e.g. a templated repository name) means
    // the image can't be resolved statically — no tags to offer. This also
    // covers a `:` inside such a placeholder landing the split mid-token.
    if base.contains("${") {
        debug!("variant detection: base image {base:?} is not static");
        return None;
    }

    // Find the last ${templateOption:KEY} in the tag portion.
    let pattern = "${templateOption:";
    let mut last_key = None;
    let mut search_from = 0;
    while let Some(start) = tag_portion[search_from..].find(pattern) {
        let key_start = search_from + start + pattern.len();
        if let Some(end) = tag_portion[key_start..].find('}') {
            last_key = Some(tag_portion[key_start..key_start + end].to_owned());
            search_from = key_start + end + 1;
        } else {
            break;
        }
    }

    Some(ImageVariantInfo {
        base_image: base.to_owned(),
        option_key: last_key?,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[expect(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // detect_image_variant_option
    // -----------------------------------------------------------------------

    #[test]
    fn detect_variant_simple_image() {
        let config =
            r#"{"image": "mcr.microsoft.com/devcontainers/rust:1-${templateOption:imageVariant}"}"#;
        let info = detect_image_variant_option(config).unwrap();
        assert_eq!(info.base_image, "mcr.microsoft.com/devcontainers/rust");
        assert_eq!(info.option_key, "imageVariant");
    }

    #[test]
    fn detect_variant_multiple_options_returns_last() {
        let config = r#"{"image": "mcr.microsoft.com/devcontainers/typescript-node:${templateOption:nodeVersion}-${templateOption:imageVariant}"}"#;
        let info = detect_image_variant_option(config).unwrap();
        assert_eq!(
            info.base_image,
            "mcr.microsoft.com/devcontainers/typescript-node"
        );
        assert_eq!(info.option_key, "imageVariant");
    }

    #[test]
    fn detect_variant_jsonc_with_comments() {
        // Regression: official templates ship JSONC with `//` comments.
        // Plain JSON parsing failed silently, so the version picker never
        // appeared during `cella init`. Mirrors the real typescript-node
        // template content.
        let config = r#"
// For format details, see https://aka.ms/devcontainer.json.
{
	"name": "Node.js & TypeScript",
	// Or use a Dockerfile or Docker Compose file.
	"image": "mcr.microsoft.com/devcontainers/typescript-node:4-${templateOption:imageVariant}"

	// Features to add to the dev container. More info: https://containers.dev/features.
	// "features": {},
}
"#;
        let info = detect_image_variant_option(config).unwrap();
        assert_eq!(
            info.base_image,
            "mcr.microsoft.com/devcontainers/typescript-node"
        );
        assert_eq!(info.option_key, "imageVariant");
    }

    #[test]
    fn detect_variant_jsonc_trailing_comma_before_comments() {
        // Regression: a trailing comma after the last real property,
        // followed only by commented-out properties, must not break
        // detection.
        let config = "{\n\t\"name\": \"Go\",\n\t\"image\": \"mcr.microsoft.com/devcontainers/go:1-${templateOption:imageVariant}\",\n\t// \"features\": {},\n}";
        let info = detect_image_variant_option(config).unwrap();
        assert_eq!(info.base_image, "mcr.microsoft.com/devcontainers/go");
        assert_eq!(info.option_key, "imageVariant");
    }

    #[test]
    fn detect_variant_templated_repository_is_skipped() {
        // A placeholder in the repository segment makes the naive tag split
        // land inside `${templateOption:flavor}`; there is no static base
        // image to list tags for, so detection must bail instead of
        // returning a mangled reference.
        let config =
            r#"{"image": "ghcr.io/org/${templateOption:flavor}:1-${templateOption:imageVariant}"}"#;
        assert!(detect_image_variant_option(config).is_none());
    }

    #[test]
    fn detect_variant_no_image_field() {
        let config = r#"{"build": {"dockerfile": "Dockerfile"}}"#;
        assert!(detect_image_variant_option(config).is_none());
    }

    #[test]
    fn detect_variant_no_template_option_in_tag() {
        let config = r#"{"image": "ubuntu:latest"}"#;
        assert!(detect_image_variant_option(config).is_none());
    }

    #[test]
    fn detect_variant_no_tag_at_all() {
        let config = r#"{"image": "ubuntu"}"#;
        assert!(detect_image_variant_option(config).is_none());
    }
}
