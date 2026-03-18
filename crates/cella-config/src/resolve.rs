//! Config resolution: discover, parse, merge layers, compute hash.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::diagnostic::ConfigDiagnostic;
use crate::discover::discover_config;
use crate::error::CellaConfigError;
use crate::jsonc;
use crate::merge::merge_layers;
use crate::parse::parse_devcontainer;

/// Fully resolved devcontainer configuration.
pub struct ResolvedConfig {
    /// Merged config as JSON Value.
    pub config: serde_json::Value,
    /// Path to the primary devcontainer.json that was discovered.
    pub config_path: PathBuf,
    /// Workspace root directory.
    pub workspace_root: PathBuf,
    /// SHA256 hex of the canonical JSON serialization of the merged config.
    pub config_hash: String,
    /// Diagnostics (warnings) from parsing.
    pub warnings: Vec<ConfigDiagnostic>,
}

/// Resolve devcontainer configuration from a workspace root.
///
/// Discovers, parses, merges layers (global → workspace → local),
/// and computes a config hash.
///
/// # Errors
///
/// Returns `CellaConfigError` if discovery fails, a file cannot be read,
/// or JSONC/JSON parsing fails.
pub fn resolve_config(workspace_root: &Path) -> Result<ResolvedConfig, CellaConfigError> {
    let config_path = discover_config(workspace_root)?;

    let raw_text =
        std::fs::read_to_string(&config_path).map_err(|source| CellaConfigError::ReadFile {
            path: config_path.display().to_string(),
            source,
        })?;

    // Parse for validation warnings (non-strict mode)
    let warnings = match parse_devcontainer(&config_path.display().to_string(), &raw_text, false) {
        Ok((_config, warnings)) => warnings,
        Err(diags) => diags.diagnostics().to_vec(),
    };

    // Get raw JSON Value for merging (strip JSONC, parse to Value)
    let cleaned =
        jsonc::strip_jsonc(&raw_text).map_err(|e| CellaConfigError::Jsonc(e.to_string()))?;
    let mut config: serde_json::Value = serde_json::from_str(&cleaned)?;

    // Merge global config if exists (~/.config/cella/global.jsonc)
    if let Some(home) = home_dir() {
        let global_path = home.join(".config/cella/global.jsonc");
        if global_path.is_file() {
            debug!("merging global config from {}", global_path.display());
            let global_value = read_jsonc_value(&global_path)?;
            let mut merged = global_value;
            merge_layers(&mut merged, &config);
            config = merged;
        }
    }

    // Merge local override if exists
    let local_path = workspace_root
        .join(".devcontainer")
        .join("devcontainer.local.jsonc");
    if local_path.is_file() {
        debug!("merging local override from {}", local_path.display());
        let local_value = read_jsonc_value(&local_path)?;
        merge_layers(&mut config, &local_value);
    }

    // Compute hash of canonical JSON
    let canonical = serde_json::to_string(&config)?;
    let hash = hex::encode(Sha256::digest(canonical.as_bytes()));

    Ok(ResolvedConfig {
        config,
        config_path,
        workspace_root: workspace_root.to_path_buf(),
        config_hash: hash,
        warnings,
    })
}

/// Read a JSONC file and return it as a `serde_json::Value`.
fn read_jsonc_value(path: &Path) -> Result<serde_json::Value, CellaConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| CellaConfigError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let cleaned = jsonc::strip_jsonc(&raw).map_err(|e| CellaConfigError::Jsonc(e.to_string()))?;
    let value = serde_json::from_str(&cleaned)?;
    Ok(value)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_devcontainer(workspace: &Path, content: &str) {
        let dir = workspace.join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("devcontainer.json"), content).unwrap();
    }

    #[test]
    fn resolve_minimal_config() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);

        let resolved = resolve_config(tmp.path()).unwrap();
        assert_eq!(resolved.config["image"], "ubuntu");
        assert!(!resolved.config_hash.is_empty());
    }

    #[test]
    fn resolve_config_hash_deterministic() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);

        let r1 = resolve_config(tmp.path()).unwrap();
        let r2 = resolve_config(tmp.path()).unwrap();
        assert_eq!(r1.config_hash, r2.config_hash);
    }

    #[test]
    fn resolve_config_hash_changes() {
        let tmp1 = TempDir::new().unwrap();
        create_devcontainer(tmp1.path(), r#"{"image": "ubuntu"}"#);

        let tmp2 = TempDir::new().unwrap();
        create_devcontainer(tmp2.path(), r#"{"image": "alpine"}"#);

        let r1 = resolve_config(tmp1.path()).unwrap();
        let r2 = resolve_config(tmp2.path()).unwrap();
        assert_ne!(r1.config_hash, r2.config_hash);
    }

    #[test]
    fn resolve_hash_ignores_whitespace() {
        let tmp1 = TempDir::new().unwrap();
        create_devcontainer(tmp1.path(), r#"{"image": "ubuntu"}"#);

        let tmp2 = TempDir::new().unwrap();
        create_devcontainer(
            tmp2.path(),
            r#"{
            "image": "ubuntu"
        }"#,
        );

        let r1 = resolve_config(tmp1.path()).unwrap();
        let r2 = resolve_config(tmp2.path()).unwrap();
        assert_eq!(r1.config_hash, r2.config_hash);
    }

    #[test]
    fn resolve_not_found() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_config(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn resolve_with_local_override() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu", "remoteUser": "root"}"#);

        let local_path = tmp
            .path()
            .join(".devcontainer")
            .join("devcontainer.local.jsonc");
        std::fs::write(&local_path, r#"{"remoteUser": "vscode"}"#).unwrap();

        let resolved = resolve_config(tmp.path()).unwrap();
        assert_eq!(resolved.config["remoteUser"], "vscode");
        assert_eq!(resolved.config["image"], "ubuntu");
    }
}
