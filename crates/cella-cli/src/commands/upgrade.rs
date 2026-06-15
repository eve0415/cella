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

    /// Target a single Feature for upgrade (the official CLI's dependabot path,
    /// alongside `--target-version`). Hidden parity flag, accepted-and-ignored:
    /// cella currently refreshes the whole lockfile, so targeted single-Feature
    /// upgrade is a follow-up. Accepted independently of `--target-version`.
    #[arg(short = 'f', long = "feature", hide = true)]
    pub feature: Option<String>,

    /// The target version (`x`, `x.y`, or `x.y.z`) for `--feature`. Hidden
    /// parity flag, accepted-and-ignored (see `--feature`); accepted
    /// independently.
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
        let config_path =
            resolve_config_path(self.workspace_folder.as_ref(), self.config.as_ref())?;
        let config_json = read_config_json(&config_path)?;
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

    #[test]
    fn upgrade_accepts_official_flags() {
        // Regression: `--docker-path`, `--docker-compose-path`, `--feature`
        // (alias -f) and `--target-version` (alias -v) are official
        // featuresUpgradeOptions flags that were missing. They must all parse.
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
