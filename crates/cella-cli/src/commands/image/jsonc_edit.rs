//! Comment-preserving rewrite of the `"image"` value in devcontainer.json.
//!
//! Separate from `features::jsonc_edit` on purpose: every `FeatureEdit`
//! variant is keyed by a feature reference, and this edits a scalar at the
//! config root.

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};

/// Replace the root `"image"` value, leaving all comments and formatting intact.
///
/// # Errors
///
/// Returns an error if the source is not parseable JSONC, if its root is not
/// an object, or if it has no `"image"` property to replace.
pub fn set_image(
    source: &str,
    new_image: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let root = CstRootNode::parse(source, &ParseOptions::default())
        .map_err(|e| format!("failed to parse JSONC: {e}"))?;
    let root_obj = root
        .object_value()
        .ok_or("devcontainer.json root is not an object")?;
    let prop = root_obj
        .get("image")
        .ok_or("devcontainer.json has no \"image\" property")?;
    prop.set_value(CstInputValue::String(new_image.to_owned()));
    Ok(root.to_string())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_image_preserving_comments() {
        let src = r#"{
  // our base image
  "name": "cella",
  "image": "mcr.microsoft.com/devcontainers/rust:2.0.9-trixie", // pinned
  "features": {}
}"#;
        let out = set_image(src, "mcr.microsoft.com/devcontainers/rust:2.0.14-trixie").unwrap();
        assert!(out.contains("// our base image"));
        assert!(out.contains("// pinned"));
        assert!(out.contains("rust:2.0.14-trixie"));
        assert!(!out.contains("2.0.9-trixie"));
    }

    #[test]
    fn handles_a_leading_comment_before_the_root_brace() {
        // Settles whether object_value() suffices, or whether this needs
        // object_value_or_set() the way features/jsonc_edit.rs uses.
        let src = "// top of file\n{\n  \"image\": \"ubuntu:24.04\"\n}";
        let out = set_image(src, "ubuntu:24.10").unwrap();
        assert!(out.starts_with("// top of file"));
        assert!(out.contains("ubuntu:24.10"));
    }

    #[test]
    fn errors_when_there_is_no_image_key() {
        assert!(set_image(r#"{"name": "x"}"#, "ubuntu:24.04").is_err());
    }

    #[test]
    fn errors_when_the_root_is_not_an_object() {
        // A non-object root has no "image" to replace; creating one would be
        // inventing a config, not updating it.
        assert!(set_image("[1, 2, 3]", "ubuntu:24.04").is_err());
        assert!(set_image("\"just a string\"", "ubuntu:24.04").is_err());
        assert!(set_image("{ not json", "ubuntu:24.04").is_err());
    }

    #[test]
    fn preserves_tabs_and_trailing_commas() {
        // Real devcontainer.json files ship JSONC with trailing commas; the
        // rewrite must not normalise the file out from under the user.
        let src = "{\n\t\"image\": \"ubuntu:24.04\",\n\t// \"features\": {},\n}";
        let out = set_image(src, "ubuntu:24.10").unwrap();
        assert!(out.contains('\t'), "indentation must survive: {out}");
        assert!(out.contains("// \"features\": {},"));
        assert!(out.contains("ubuntu:24.10"));
    }
}
