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
