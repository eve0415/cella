//! Image tag fetching, filtering, and sorting for version pinning.
//!
//! Enables users to pin a devcontainer to a specific image version
//! (e.g. `4.0.6-22-trixie`) rather than using the template's default
//! tag pattern.

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::secrets::RegistryAuth;
use tracing::debug;

use crate::cache::TemplateCache;
use crate::error::TemplateError;

/// Maximum number of pinned tags to present to the user.
pub const MAX_PINNED_TAGS: usize = 15;

/// Information about the image variant option detected in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageVariantInfo {
    /// Base image without tag (e.g. `mcr.microsoft.com/devcontainers/rust`).
    pub base_image: String,
    /// Template option key that controls the variant (e.g. `imageVariant`).
    pub option_key: String,
}

/// Detect which template option controls the image variant by parsing the
/// template's `devcontainer.json` content (as raw JSONC-stripped JSON).
///
/// Looks for `${templateOption:KEY}` in the `"image"` field value. Returns
/// the last option reference found in the tag portion (after `:`).
pub fn detect_image_variant_option(config_content: &str) -> Option<ImageVariantInfo> {
    let parsed: serde_json::Value = serde_json::from_str(config_content).ok()?;
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

/// Parse an image reference into `(registry, repository)`.
///
/// # Errors
///
/// Returns [`TemplateError::TagFetchFailed`] if the reference has no `/`.
pub fn parse_image_ref(image: &str) -> Result<(String, String), TemplateError> {
    image
        .split_once('/')
        .map(|(reg, repo)| (reg.to_owned(), repo.to_owned()))
        .ok_or_else(|| TemplateError::TagFetchFailed {
            image: image.to_owned(),
            message: "invalid image reference: expected registry/repository".to_owned(),
        })
}

/// Filter tags to those ending with `-{suffix}` that have a version prefix.
///
/// Bare codenames (e.g. just `"trixie"`) are excluded — only tags with a
/// leading numeric component before the suffix are returned.
pub fn filter_tags_by_suffix<'a>(tags: &[&'a str], suffix: &str) -> Vec<&'a str> {
    let needle = format!("-{suffix}");
    tags.iter()
        .copied()
        .filter(|tag| tag.ends_with(&needle) && tag.len() > needle.len())
        .collect()
}

/// Parse leading dot-and-dash-separated numbers from a tag for sorting.
///
/// Stops at the first segment that cannot be parsed as `u64`.
///
/// # Examples
///
/// - `"4.0.6-trixie"` → `[4, 0, 6]`
/// - `"4.0.6-22-trixie"` → `[4, 0, 6, 22]`
/// - `"22-trixie"` → `[22]`
/// - `"latest"` → `[]`
pub fn parse_version_key(tag: &str) -> Vec<u64> {
    let mut nums = Vec::new();
    for segment in tag.split(['.', '-']) {
        if let Ok(n) = segment.parse::<u64>() {
            nums.push(n);
        } else {
            break;
        }
    }
    nums
}

/// Sort tags in descending version order.
pub fn sort_tags_descending(tags: &mut [&str]) {
    tags.sort_by(|a, b| {
        let a_key = parse_version_key(a);
        let b_key = parse_version_key(b);
        b_key.cmp(&a_key)
    });
}

/// Fetch available tags for an image from its OCI registry.
///
/// # Errors
///
/// Returns [`TemplateError::TagFetchFailed`] on network or API errors.
pub async fn fetch_image_tags(
    image_ref: &str,
    cache: &TemplateCache,
    force_refresh: bool,
) -> Result<Vec<String>, TemplateError> {
    if !force_refresh && let Some(tags) = cache.get_image_tags(image_ref) {
        return Ok(tags);
    }

    let (registry, repository) = parse_image_ref(image_ref)?;

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);

    let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), "latest".to_owned());
    let auth = build_registry_auth(&registry);

    debug!("fetching image tags for {image_ref}");

    let response = client
        .list_tags(&oci_ref, &auth, None, None)
        .await
        .map_err(|e| TemplateError::TagFetchFailed {
            image: image_ref.to_owned(),
            message: format!("failed to list tags: {e}"),
        })?;

    let _ = cache.put_image_tags(image_ref, &response.tags);

    Ok(response.tags)
}

/// Build [`RegistryAuth`] from Docker credential store.
fn build_registry_auth(registry: &str) -> RegistryAuth {
    let creds = cella_features::auth::resolve_credentials(registry);
    if let (Some(u), Some(p)) = (creds.username, creds.password) {
        debug!("using basic auth for {registry}");
        RegistryAuth::Basic(u, p)
    } else {
        debug!("no credentials for {registry}; using anonymous auth");
        RegistryAuth::Anonymous
    }
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

    // -----------------------------------------------------------------------
    // parse_image_ref
    // -----------------------------------------------------------------------

    #[test]
    fn parse_image_ref_mcr() {
        let (reg, repo) = parse_image_ref("mcr.microsoft.com/devcontainers/rust").unwrap();
        assert_eq!(reg, "mcr.microsoft.com");
        assert_eq!(repo, "devcontainers/rust");
    }

    #[test]
    fn parse_image_ref_ghcr() {
        let (reg, repo) = parse_image_ref("ghcr.io/myorg/myimage").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "myorg/myimage");
    }

    #[test]
    fn parse_image_ref_invalid() {
        assert!(parse_image_ref("no-slash").is_err());
    }

    // -----------------------------------------------------------------------
    // filter_tags_by_suffix
    // -----------------------------------------------------------------------

    #[test]
    fn filter_tags_matching_codename() {
        let tags = vec![
            "4.0.6-trixie",
            "4.0.5-trixie",
            "4.0.6-bookworm",
            "1-trixie",
            "latest",
            "trixie",
        ];
        let filtered = filter_tags_by_suffix(&tags, "trixie");
        assert!(filtered.contains(&"4.0.6-trixie"));
        assert!(filtered.contains(&"4.0.5-trixie"));
        assert!(filtered.contains(&"1-trixie"));
        assert!(!filtered.contains(&"4.0.6-bookworm"));
        assert!(!filtered.contains(&"latest"));
        assert!(!filtered.contains(&"trixie")); // bare codename excluded
    }

    #[test]
    fn filter_tags_no_matches() {
        let tags = vec!["latest", "bookworm"];
        let filtered = filter_tags_by_suffix(&tags, "trixie");
        assert!(filtered.is_empty());
    }

    // -----------------------------------------------------------------------
    // parse_version_key
    // -----------------------------------------------------------------------

    #[test]
    fn version_key_full_semver() {
        assert_eq!(parse_version_key("4.0.6-trixie"), vec![4, 0, 6]);
    }

    #[test]
    fn version_key_with_extra_number() {
        assert_eq!(parse_version_key("4.0.6-22-trixie"), vec![4, 0, 6, 22]);
    }

    #[test]
    fn version_key_major_only() {
        assert_eq!(parse_version_key("22-trixie"), vec![22]);
    }

    #[test]
    fn version_key_no_numbers() {
        assert_eq!(parse_version_key("latest"), Vec::<u64>::new());
    }

    // -----------------------------------------------------------------------
    // sort_tags_descending
    // -----------------------------------------------------------------------

    #[test]
    fn sort_descending_mixed_versions() {
        let mut tags = vec![
            "4.0.5-trixie",
            "4.0.6-trixie",
            "3.9.0-trixie",
            "4.0.6-22-trixie",
        ];
        sort_tags_descending(&mut tags);
        assert_eq!(tags[0], "4.0.6-22-trixie");
        assert_eq!(tags[1], "4.0.6-trixie");
        assert_eq!(tags[2], "4.0.5-trixie");
        assert_eq!(tags[3], "3.9.0-trixie");
    }

    #[test]
    fn sort_descending_same_prefix() {
        let mut tags = vec!["1-trixie", "22-trixie", "20-trixie"];
        sort_tags_descending(&mut tags);
        assert_eq!(tags, vec!["22-trixie", "20-trixie", "1-trixie"]);
    }

    // -----------------------------------------------------------------------
    // Image tag cache
    // -----------------------------------------------------------------------

    #[test]
    fn image_tag_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let tags = vec!["4.0.6-trixie".to_owned(), "4.0.5-trixie".to_owned()];

        cache
            .put_image_tags("mcr.microsoft.com/devcontainers/rust", &tags)
            .unwrap();
        let cached = cache
            .get_image_tags("mcr.microsoft.com/devcontainers/rust")
            .unwrap();
        assert_eq!(cached, tags);
    }

    #[test]
    fn image_tag_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        assert!(cache.get_image_tags("nonexistent").is_none());
    }
}
