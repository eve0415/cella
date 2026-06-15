//! `cella upgrade` — refresh the feature lockfile without starting a container.

use std::path::{Path, PathBuf};

use clap::Args;

use cella_features::{
    BaseImageContext, FeatureCache, LockfilePolicy,
    lockfile::{lockfile_path, write_lockfile},
    oci::detect_platform,
};

/// Upgrade (refresh) the devcontainer feature lockfile.
///
/// Resolves all OCI features fresh from the registry and writes an updated
/// `devcontainer-lock.json`. Does not require a running Docker daemon.
#[derive(Args)]
pub struct UpgradeArgs {
    /// Workspace folder path (defaults to current directory).
    #[arg(long)]
    pub workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Print the new lockfile to stdout without writing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Path to the `docker` executable (compatibility no-op; cella talks to the
    /// engine API directly). Matches the official `--docker-path` default.
    #[arg(long = "docker-path", default_value = "docker")]
    pub docker_path: String,

    /// Path to the `docker-compose` executable (compatibility no-op). Matches
    /// the official `--docker-compose-path` default.
    #[arg(long = "docker-compose-path", default_value = "docker-compose")]
    pub docker_compose_path: String,

    /// Pin a single Feature's version in devcontainer.json, then refresh the
    /// lockfile (the official CLI's dependabot path). Must be used together with
    /// `--target-version`. Hidden, matching the official flag.
    #[arg(short = 'f', long = "feature", hide = true)]
    pub feature: Option<String>,

    /// The version (`x`, `x.y`, or `x.y.z`) to pin `--feature` to in
    /// devcontainer.json. Must be used together with `--feature`. Hidden,
    /// matching the official flag.
    #[arg(short = 'v', long = "target-version", hide = true)]
    pub target_version: Option<String>,

    /// Log verbosity level.
    #[arg(long, value_enum, default_value_t = crate::commands::LogLevel::Info)]
    pub log_level: crate::commands::LogLevel,
}

impl UpgradeArgs {
    /// Execute the upgrade command.
    ///
    /// # Errors
    ///
    /// Returns an error if config discovery, parsing, or feature resolution fails.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Paired-use + version-format validation runs first, before config
        // discovery — matching the official yargs `.check()` (parse-time). So
        // `upgrade --feature X` (no `--target-version`) in a folder without a
        // devcontainer.json reports the paired-use error, not "no config found".
        validate_feature_target(self.feature.as_deref(), self.target_version.as_deref())?;

        let config_path =
            resolve_config_path(self.workspace_folder.as_ref(), self.config.as_ref())?;
        let mut config_json = read_config_json(&config_path)?;

        // Dependabot path: pin the requested Feature version in devcontainer.json
        // before resolving features / writing the lockfile. Mirrors the official
        // `featuresUpgrade` — the config write happens even under `--dry-run`
        // (only the lockfile write is gated), so a single call can pin and then
        // emit the regenerated lockfile to stdout.
        if let (Some(feature), Some(target_version)) =
            (self.feature.as_deref(), self.target_version.as_deref())
        {
            apply_feature_pin(&config_path, &config_json, feature, target_version)?;
            // Re-read so the lockfile regeneration below sees the pinned version.
            config_json = read_config_json(&config_path)?;
        }

        let platform = detect_platform(std::env::consts::OS, std::env::consts::ARCH);
        let cache = FeatureCache::new();
        let base_image = config_json
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let base_ctx = BaseImageContext {
            base_image,
            image_user: "root",
            metadata: None,
            omit: cella_features::MetadataOmit::default(),
        };

        let resolved = cella_features::resolve_features(
            &config_json,
            &config_path,
            &platform,
            &cache,
            &base_ctx,
            false,
            LockfilePolicy::Upgrade,
        )
        .await
        .map_err(|e| format!("feature resolution failed: {e}"))?;

        let Some(lockfile) = &resolved.lockfile else {
            eprintln!("No OCI features found; lockfile not updated.");
            return Ok(());
        };

        if self.dry_run {
            let mut json = serde_json::to_string_pretty(lockfile)?;
            json.push('\n');
            print!("{json}");
        } else {
            write_lockfile(&config_path, lockfile)
                .map_err(|e| format!("failed to write lockfile: {e}"))?;
            eprintln!("Wrote {}", lockfile_path(&config_path).display());
        }

        Ok(())
    }
}

fn resolve_config_path(
    workspace_folder: Option<&PathBuf>,
    config_override: Option<&PathBuf>,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(config) = config_override {
        return Ok(config.clone());
    }

    let workspace = workspace_folder.cloned().map_or_else(
        || {
            std::env::current_dir()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })
        },
        Ok,
    )?;

    let candidates = [
        workspace.join(".devcontainer").join("devcontainer.json"),
        workspace.join(".devcontainer.json"),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    Err(format!("no devcontainer.json found under {}", workspace.display()).into())
}

fn read_config_json(
    config_path: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let raw = std::fs::read_to_string(config_path)?;
    let stripped = cella_jsonc::strip(&raw).map_err(|e| e.to_string())?;
    let value = serde_json::from_str(&stripped)?;
    Ok(value)
}

/// Validate the paired `--feature`/`--target-version` constraints.
///
/// Mirrors the official `featuresUpgradeOptions().check()`: both flags must be
/// supplied together, and `--target-version` must be `x`, `x.y`, or `x.y.z`
/// (one to three dot-separated numeric parts). Error strings match verbatim.
fn validate_feature_target(
    feature: Option<&str>,
    target_version: Option<&str>,
) -> Result<(), String> {
    if feature.is_some() != target_version.is_some() {
        return Err(
            "The '--target-version' and '--feature' flag must be used together.".to_owned(),
        );
    }
    if let Some(version) = target_version
        && !is_valid_target_version(version)
    {
        return Err(format!(
            "Invalid version '{version}'.  Must be in the form of 'x', 'x.y', or 'x.y.z'"
        ));
    }
    Ok(())
}

/// Match the official `/^\d+(\.\d+(\.\d+)?)?$/`: one to three dot-separated
/// parts, each a non-empty run of ASCII digits.
fn is_valid_target_version(version: &str) -> bool {
    let mut parts = 0u8;
    for part in version.split('.') {
        parts += 1;
        if parts > 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    true
}

/// Strip a trailing `:tag` or `@digest` from a Feature id, matching the official
/// `getFeatureIdWithoutVersion` (`/[:@][^/]*$/`). The delimiter must appear
/// after the last `/`, so registry ports (`host:5000/repo`) survive.
fn feature_id_without_version(id: &str) -> &str {
    let start = id.rfind('/').map_or(0, |i| i + 1);
    id[start..]
        .find([':', '@'])
        .map_or(id, |rel| &id[..start + rel])
}

/// Outcome of pinning a Feature version into the raw devcontainer.json text.
enum PinOutcome {
    /// The config declares no `features` object.
    NoFeatures,
    /// No matching Feature, or the text is already at the target version.
    NoChange,
    /// The config text with the matched Feature key re-pinned.
    Changed(String),
}

/// Re-pin `target_feature` to `target_version` in the raw config text.
///
/// Mirrors the official `updateFeatureVersionInConfig`: find the user Feature
/// whose id-without-version matches the target, then rewrite its key to
/// `<id-without-version>:<target_version>`.
///
/// Deliberate bug-fix vs upstream: the official builds `new RegExp(current)` and
/// does an unanchored global replace, so `.` acts as a wildcard and a bare id
/// corrupts longer ids sharing its prefix (`.../node` rewrites inside
/// `.../nodejs`). cella replaces the quoted key token `"<id>"` literally —
/// byte-identical for normal configs, but it never corrupts prefixes.
fn pin_feature_version_in_text(
    raw: &str,
    config: &serde_json::Value,
    target_feature: &str,
    target_version: &str,
) -> PinOutcome {
    let Some(features) = config
        .get("features")
        .and_then(serde_json::Value::as_object)
    else {
        return PinOutcome::NoFeatures;
    };
    let target_no_version = feature_id_without_version(target_feature);
    let Some(user_id) = features
        .keys()
        .find(|key| feature_id_without_version(key) == target_no_version)
    else {
        return PinOutcome::NoChange;
    };
    let new_key = format!("{target_no_version}:{target_version}");
    let updated = raw.replace(&format!("\"{user_id}\""), &format!("\"{new_key}\""));
    if updated == raw {
        PinOutcome::NoChange
    } else {
        PinOutcome::Changed(updated)
    }
}

/// Pin the requested Feature version in devcontainer.json and write it back.
///
/// Emits the same progress lines as the official `featuresUpgrade`. The write is
/// unconditional (not gated by `--dry-run`) to match upstream behavior.
fn apply_feature_pin(
    config_path: &Path,
    config_json: &serde_json::Value,
    feature: &str,
    target_version: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    eprintln!("Updating '{feature}' to '{target_version}' in devcontainer.json");
    let raw = std::fs::read_to_string(config_path)?;
    match pin_feature_version_in_text(&raw, config_json, feature, target_version) {
        PinOutcome::NoFeatures => {
            eprintln!("No Features found in '{}'.", config_path.display());
        }
        PinOutcome::NoChange => {
            tracing::trace!("No changes to config file: {}", config_path.display());
        }
        PinOutcome::Changed(updated) => {
            eprintln!("Updating config file: '{}'", config_path.display());
            std::fs::write(config_path, updated)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    fn upgrade_args(extra: &[&str]) -> super::UpgradeArgs {
        let mut argv = vec!["cella", "upgrade"];
        argv.extend_from_slice(extra);
        let cli = crate::Cli::try_parse_from(argv).expect("upgrade args parse");
        match cli.command {
            crate::commands::Command::Upgrade(args) => args,
            _ => panic!("expected upgrade command"),
        }
    }

    fn cfg(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("test config parses")
    }

    #[test]
    fn validate_requires_both_or_neither() {
        assert!(super::validate_feature_target(None, None).is_ok());
        assert!(super::validate_feature_target(Some("node"), Some("1")).is_ok());

        let only_feature = super::validate_feature_target(Some("node"), None).unwrap_err();
        let only_version = super::validate_feature_target(None, Some("1")).unwrap_err();
        let expected = "The '--target-version' and '--feature' flag must be used together.";
        assert_eq!(only_feature, expected);
        assert_eq!(only_version, expected);
    }

    #[test]
    fn validate_rejects_malformed_version() {
        // Exact official message, including the double space after the period.
        let err = super::validate_feature_target(Some("node"), Some("1.2.3.4")).unwrap_err();
        assert_eq!(
            err,
            "Invalid version '1.2.3.4'.  Must be in the form of 'x', 'x.y', or 'x.y.z'"
        );
    }

    #[test]
    fn target_version_format_matches_official_regex() {
        for ok in ["0", "1", "10", "1.2", "1.2.3", "01", "1.0.0"] {
            assert!(super::is_valid_target_version(ok), "{ok} should be valid");
        }
        for bad in [
            "", "1.", ".1", "1..2", "1.2.3.4", "1.x", "v1", "1.2.", "latest",
        ] {
            assert!(
                !super::is_valid_target_version(bad),
                "{bad} should be invalid"
            );
        }
    }

    #[test]
    fn feature_id_without_version_matches_official() {
        let strip = super::feature_id_without_version;
        assert_eq!(strip("node:1"), "node");
        assert_eq!(strip("ghcr.io/x/node:2"), "ghcr.io/x/node");
        assert_eq!(strip("ghcr.io/x/node"), "ghcr.io/x/node");
        // Registry port (`:5000`) is before the last `/`, so it survives.
        assert_eq!(strip("host:5000/x/node"), "host:5000/x/node");
        assert_eq!(strip("host:5000/x/node:2"), "host:5000/x/node");
        // `@digest` delimiter, matching the official `/[:@][^/]*$/`.
        assert_eq!(strip("node@sha256:abc"), "node");
    }

    #[test]
    fn pin_no_features_object() {
        assert!(matches!(
            super::pin_feature_version_in_text(r"{}", &cfg(r"{}"), "node", "2"),
            super::PinOutcome::NoFeatures
        ));
    }

    #[test]
    fn pin_empty_features_is_no_change() {
        let raw = r#"{"features":{}}"#;
        assert!(matches!(
            super::pin_feature_version_in_text(raw, &cfg(raw), "ghcr.io/x/node", "2"),
            super::PinOutcome::NoChange
        ));
    }

    #[test]
    fn pin_normal_case_rewrites_version() {
        let raw = "{\n  \"features\": {\n    \"ghcr.io/x/node:1\": {}\n  }\n}\n";
        let super::PinOutcome::Changed(updated) =
            super::pin_feature_version_in_text(raw, &cfg(raw), "ghcr.io/x/node", "2")
        else {
            panic!("expected Changed");
        };
        // Identical to what the official regex would produce for this case.
        let expected = "{\n  \"features\": {\n    \"ghcr.io/x/node:2\": {}\n  }\n}\n";
        assert_eq!(updated, expected);
    }

    #[test]
    fn pin_does_not_corrupt_prefix_sharing_ids() {
        // The official `new RegExp("ghcr.io/x/node")` would rewrite the prefix
        // inside `ghcr.io/x/nodejs` too. cella's quoted-key replace must not.
        let raw = "{\n  \"features\": {\n    \"ghcr.io/x/node\": {},\n    \"ghcr.io/x/nodejs\": {}\n  }\n}\n";
        let super::PinOutcome::Changed(updated) =
            super::pin_feature_version_in_text(raw, &cfg(raw), "ghcr.io/x/node", "2")
        else {
            panic!("expected Changed");
        };
        assert!(updated.contains("\"ghcr.io/x/node:2\""));
        assert!(
            updated.contains("\"ghcr.io/x/nodejs\""),
            "sibling feature must stay intact, got: {updated}"
        );
        assert!(
            !updated.contains("nodejs:2"),
            "prefix bug regression: {updated}"
        );
    }

    #[test]
    fn pin_no_matching_feature_is_no_change() {
        let raw = r#"{"features":{"ghcr.io/x/go:1":{}}}"#;
        assert!(matches!(
            super::pin_feature_version_in_text(raw, &cfg(raw), "ghcr.io/x/node", "2"),
            super::PinOutcome::NoChange
        ));
    }

    #[test]
    fn pin_already_at_target_is_no_change() {
        let raw = r#"{"features":{"ghcr.io/x/node:2":{}}}"#;
        assert!(matches!(
            super::pin_feature_version_in_text(raw, &cfg(raw), "ghcr.io/x/node", "2"),
            super::PinOutcome::NoChange
        ));
    }

    #[test]
    fn apply_feature_pin_writes_config_file() {
        // Proves the config write happens (the dependabot side effect that is
        // NOT gated by --dry-run). Pure FS, no registry.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devcontainer.json");
        let raw = "{\n  \"features\": {\n    \"ghcr.io/x/node:1\": {}\n  }\n}\n";
        std::fs::write(&path, raw).expect("write seed");
        let config = cfg(raw);

        super::apply_feature_pin(&path, &config, "ghcr.io/x/node", "2").expect("pin");

        let after = std::fs::read_to_string(&path).expect("read back");
        assert!(after.contains("\"ghcr.io/x/node:2\""), "got: {after}");
    }

    #[test]
    fn upgrade_accepts_official_flags() {
        // Regression: `--docker-path`, `--docker-compose-path`, `--feature`
        // (alias -f) and `--target-version` (alias -v) are official
        // featuresUpgradeOptions flags. They must all parse.
        let args = upgrade_args(&[
            "--docker-path",
            "/x",
            "--docker-compose-path",
            "/y",
            "--feature",
            "ghcr.io/devcontainers/features/node",
            "-v",
            "1",
        ]);
        assert_eq!(args.docker_path, "/x");
        assert_eq!(args.docker_compose_path, "/y");
        assert_eq!(
            args.feature.as_deref(),
            Some("ghcr.io/devcontainers/features/node")
        );
        assert_eq!(args.target_version.as_deref(), Some("1"));
    }

    #[test]
    fn upgrade_flag_aliases_and_defaults_match_official() {
        // -f is the alias for --feature; defaults mirror the official CLI.
        let args = upgrade_args(&["-f", "node"]);
        assert_eq!(args.feature.as_deref(), Some("node"));

        let args = upgrade_args(&[]);
        assert_eq!(args.docker_path, "docker");
        assert_eq!(args.docker_compose_path, "docker-compose");
        assert!(args.feature.is_none());
        assert!(args.target_version.is_none());
    }
}
