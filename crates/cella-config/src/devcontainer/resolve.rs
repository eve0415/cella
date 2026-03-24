//! Config resolution: discover, parse, merge layers, compute hash.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::CellaConfigError;
use crate::diagnostic::{Diagnostic, Severity};
use crate::discover;
use crate::jsonc;
use crate::merge;
use crate::parse;

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
    pub warnings: Vec<Diagnostic>,
}

/// Resolve devcontainer configuration from a workspace root.
///
/// Discovers, parses, merges layers (global → workspace → local),
/// and computes a config hash.
///
/// When `config_path_override` is provided, discovery is skipped and the
/// given path is used directly.
///
/// # Errors
///
/// Returns `CellaConfigError` if discovery fails, a file cannot be read,
/// or JSONC/JSON parsing fails.
pub fn config(
    workspace_root: &Path,
    config_path_override: Option<&Path>,
) -> Result<ResolvedConfig, CellaConfigError> {
    let config_path = if let Some(override_path) = config_path_override {
        override_path.to_path_buf()
    } else {
        discover::config(workspace_root)?
    };

    let raw_text =
        std::fs::read_to_string(&config_path).map_err(|source| CellaConfigError::ReadFile {
            path: config_path.display().to_string(),
            source,
        })?;

    // Parse for validation warnings (non-strict mode)
    let warnings = match parse::devcontainer(&config_path.display().to_string(), &raw_text, false) {
        Ok((_config, warnings)) => warnings,
        Err(diags) => diags.diagnostics().to_vec(),
    };

    // Get raw JSON Value for merging (strip JSONC, parse to Value)
    let cleaned = jsonc::strip(&raw_text).map_err(|e| CellaConfigError::Jsonc(e.to_string()))?;
    let mut config: serde_json::Value = serde_json::from_str(&cleaned)?;

    // Merge global config if exists (~/.config/cella/global.jsonc)
    if let Some(home) = home_dir() {
        let global_path = home.join(".config/cella/global.jsonc");
        if global_path.is_file() {
            debug!("merging global config from {}", global_path.display());
            let global_value = read_jsonc_value(&global_path)?;
            let mut merged = global_value;
            merge::layers(&mut merged, &config);
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
        merge::layers(&mut config, &local_value);
    }

    // Substitute variables after merge, before hash
    let container_wf = config.get("workspaceFolder").and_then(|v| v.as_str());
    let devcontainer_id = hex::encode(Sha256::digest(
        workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf())
            .to_string_lossy()
            .as_bytes(),
    ));
    let ctx = crate::subst::SubstitutionContext::new(
        workspace_root,
        container_wf,
        &devcontainer_id,
        std::env::vars().collect(),
    );
    ctx.substitute_value(&mut config);

    // Deprecation warnings for legacy properties
    let mut warnings = warnings;
    if config.get("appPort").is_some() {
        warnings.push(Diagnostic {
            severity: Severity::Warning,
            message: "\"appPort\" is deprecated. Use \"forwardPorts\" instead. Ports declared in \"appPort\" will not be bound.".into(),
            path: "$.appPort".into(),
            span: None,
            help: Some("Replace \"appPort\" with \"forwardPorts\" in your devcontainer.json".into()),
        });
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
    let cleaned = jsonc::strip(&raw).map_err(|e| CellaConfigError::Jsonc(e.to_string()))?;
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

        let resolved = config(tmp.path(), None).unwrap();
        assert_eq!(resolved.config["image"], "ubuntu");
        assert!(!resolved.config_hash.is_empty());
    }

    #[test]
    fn resolve_config_hash_deterministic() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);

        let r1 = config(tmp.path(), None).unwrap();
        let r2 = config(tmp.path(), None).unwrap();
        assert_eq!(r1.config_hash, r2.config_hash);
    }

    #[test]
    fn resolve_config_hash_changes() {
        let tmp1 = TempDir::new().unwrap();
        create_devcontainer(tmp1.path(), r#"{"image": "ubuntu"}"#);

        let tmp2 = TempDir::new().unwrap();
        create_devcontainer(tmp2.path(), r#"{"image": "alpine"}"#);

        let r1 = config(tmp1.path(), None).unwrap();
        let r2 = config(tmp2.path(), None).unwrap();
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

        let r1 = config(tmp1.path(), None).unwrap();
        let r2 = config(tmp2.path(), None).unwrap();
        assert_eq!(r1.config_hash, r2.config_hash);
    }

    #[test]
    fn resolve_not_found() {
        let tmp = TempDir::new().unwrap();
        let result = config(tmp.path(), None);
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

        let resolved = config(tmp.path(), None).unwrap();
        assert_eq!(resolved.config["remoteUser"], "vscode");
        assert_eq!(resolved.config["image"], "ubuntu");
    }

    #[test]
    fn resolve_substitutes_workspace_folder_variable() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(
            tmp.path(),
            r#"{"image": "ubuntu", "mounts": ["source=${localWorkspaceFolder}/data,target=/data"]}"#,
        );

        let resolved = config(tmp.path(), None).unwrap();
        let mount = resolved.config["mounts"][0].as_str().unwrap();

        // Should not contain the variable anymore
        assert!(!mount.contains("${localWorkspaceFolder}"));
        // Should contain the actual temp directory path
        assert!(mount.contains("/data,target=/data"));
    }

    #[test]
    fn resolve_app_port_emits_deprecation_warning() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu", "appPort": 3000}"#);

        let resolved = config(tmp.path(), None).unwrap();
        assert!(
            resolved
                .warnings
                .iter()
                .any(|w| w.message.contains("appPort") && w.message.contains("deprecated")),
            "expected deprecation warning for appPort"
        );
    }

    #[test]
    fn resolve_no_app_port_no_deprecation_warning() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);

        let resolved = config(tmp.path(), None).unwrap();
        assert!(
            !resolved
                .warnings
                .iter()
                .any(|w| w.message.contains("appPort")),
            "should not have appPort warning without appPort"
        );
    }

    #[test]
    fn resolve_hash_differs_with_different_workspace_roots() {
        let tmp1 = TempDir::new().unwrap();
        create_devcontainer(
            tmp1.path(),
            r#"{"image": "ubuntu", "mounts": ["source=${localWorkspaceFolder},target=/ws"]}"#,
        );

        let tmp2 = TempDir::new().unwrap();
        create_devcontainer(
            tmp2.path(),
            r#"{"image": "ubuntu", "mounts": ["source=${localWorkspaceFolder},target=/ws"]}"#,
        );

        let r1 = config(tmp1.path(), None).unwrap();
        let r2 = config(tmp2.path(), None).unwrap();

        // Same template but different workspace roots → different substituted values → different hashes
        assert_ne!(r1.config_hash, r2.config_hash);
    }
}
