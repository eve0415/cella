//! Config resolution: discover, parse, merge layers, compute hash.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::debug;

use super::CellaConfigError;
use super::diagnostic::{Diagnostic, Severity};
use super::discover;
use super::merge;
use super::parse;
use cella_jsonc as jsonc;

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
    /// Spec-compliant devcontainer ID (52-char base-32 string).
    pub devcontainer_id: String,
    /// Diagnostics (warnings) from parsing.
    pub warnings: Vec<Diagnostic>,
    /// Typed representation of the merged config, if validation succeeded.
    pub typed: Option<crate::schema::DevContainer>,
}

impl ResolvedConfig {
    fn config_string(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(|v| v.as_str())
    }

    pub fn name(&self) -> Option<&str> {
        self.typed
            .as_ref()
            .and_then(|t| t.name())
            .or_else(|| self.config_string("name"))
    }

    pub fn remote_user(&self) -> Option<&str> {
        self.typed
            .as_ref()
            .and_then(|t| t.remote_user())
            .or_else(|| self.config_string("remoteUser"))
    }

    pub fn container_user(&self) -> Option<&str> {
        self.typed
            .as_ref()
            .and_then(|t| t.container_user())
            .or_else(|| self.config_string("containerUser"))
    }

    pub fn features(&self) -> Option<&crate::schema::DevContainerCommonFeatures> {
        self.typed.as_ref().and_then(|t| t.features())
    }

    pub fn remote_env(&self) -> Option<&std::collections::HashMap<String, Option<String>>> {
        self.typed.as_ref().and_then(|t| t.remote_env())
    }

    pub fn mounts(&self) -> Option<&[crate::schema::DevContainerCommonMountsItem]> {
        self.typed.as_ref().and_then(|t| t.mounts())
    }

    pub fn initialize_command(
        &self,
    ) -> Option<&crate::schema::DevContainerCommonInitializeCommand> {
        self.typed.as_ref().and_then(|t| t.initialize_command())
    }

    pub fn host_requirements(&self) -> Option<&crate::schema::DevContainerCommonHostRequirements> {
        self.typed.as_ref().and_then(|t| t.host_requirements())
    }
}

/// Compute the spec-compliant `devcontainerId`.
///
/// Per <https://containers.dev/implementors/spec/#devcontainerid>:
/// SHA-256 of the sorted JSON label object, base-32 encoded, left-padded to 52 chars.
pub fn devcontainer_id(workspace_root: &Path, config_path: &Path) -> String {
    let canonical_workspace = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_config = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());

    let mut labels = BTreeMap::new();
    labels.insert(
        "devcontainer.local_folder".to_string(),
        canonical_workspace.to_string_lossy().to_string(),
    );
    labels.insert(
        "devcontainer.config_file".to_string(),
        canonical_config.to_string_lossy().to_string(),
    );
    spec_devcontainer_id(&labels)
}

fn spec_devcontainer_id(labels: &BTreeMap<String, String>) -> String {
    let json = serde_json::to_string(labels).expect("serialize labels");
    let hash = Sha256::digest(json.as_bytes());
    let base32 = bigint_to_base32(&hash);
    format!("{base32:0>52}")
}

fn bigint_to_base32(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".to_string();
    }
    let mut digits: Vec<u8> = bytes.to_vec();
    let mut result = Vec::new();
    while !(digits.is_empty() || digits.len() == 1 && digits[0] == 0) {
        let mut remainder = 0u16;
        let mut new_digits = Vec::new();
        for &d in &digits {
            let current = (remainder << 8) | u16::from(d);
            let quotient = current / 32;
            remainder = current % 32;
            if !new_digits.is_empty() || quotient > 0 {
                new_digits.push(u8::try_from(quotient).expect("quotient fits in u8"));
            }
        }
        let r = u8::try_from(remainder).expect("remainder fits in u8");
        result.push(if r < 10 { b'0' + r } else { b'a' + r - 10 });
        digits = new_digits;
    }
    if result.is_empty() {
        "0".to_string()
    } else {
        result.reverse();
        String::from_utf8(result).expect("valid utf-8")
    }
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
        Ok((_, warnings)) => warnings,
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

    // Two-pass substitution: resolve workspaceFolder first so
    // ${containerWorkspaceFolder} uses the substituted value everywhere else.
    let devcontainer_id = devcontainer_id(workspace_root, &config_path);
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let container_wf = config
        .get("workspaceFolder")
        .and_then(|v| v.as_str())
        .map(|raw| {
            if !raw.contains("${") {
                return raw.to_string();
            }
            let pre_ctx = super::subst::SubstitutionContext::new(
                workspace_root,
                None,
                &devcontainer_id,
                env.clone(),
            );
            pre_ctx.substitute_str(raw)
        });
    let ctx = super::subst::SubstitutionContext::new(
        workspace_root,
        container_wf.as_deref(),
        &devcontainer_id,
        env,
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

    let typed = match crate::schema::DevContainer::validate(&config, "") {
        Ok(t) => Some(t),
        Err(errs) => {
            debug!(
                "typed DevContainer validation failed ({} errors); typed accessors will return None",
                errs.len()
            );
            None
        }
    };

    Ok(ResolvedConfig {
        config,
        config_path,
        workspace_root: workspace_root.to_path_buf(),
        config_hash: hash,
        devcontainer_id,
        warnings,
        typed,
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
    cella_env::paths::home_dir()
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

    // --- Spec: devcontainerId computation ---
    // Reference: https://containers.dev/implementors/spec/#devcontainerid

    #[test]
    fn devcontainer_id_is_52_chars() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);
        let config_path = tmp.path().join(".devcontainer/devcontainer.json");
        let id = devcontainer_id(tmp.path(), &config_path);
        assert_eq!(id.len(), 52);
    }

    #[test]
    fn devcontainer_id_is_alphanumeric() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);
        let config_path = tmp.path().join(".devcontainer/devcontainer.json");
        let id = devcontainer_id(tmp.path(), &config_path);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn devcontainer_id_stable_across_calls() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);
        let config_path = tmp.path().join(".devcontainer/devcontainer.json");
        assert_eq!(
            devcontainer_id(tmp.path(), &config_path),
            devcontainer_id(tmp.path(), &config_path)
        );
    }

    #[test]
    fn devcontainer_id_differs_for_different_workspaces() {
        let tmp1 = TempDir::new().unwrap();
        create_devcontainer(tmp1.path(), r#"{"image": "ubuntu"}"#);
        let cfg1 = tmp1.path().join(".devcontainer/devcontainer.json");

        let tmp2 = TempDir::new().unwrap();
        create_devcontainer(tmp2.path(), r#"{"image": "ubuntu"}"#);
        let cfg2 = tmp2.path().join(".devcontainer/devcontainer.json");

        assert_ne!(
            devcontainer_id(tmp1.path(), &cfg1),
            devcontainer_id(tmp2.path(), &cfg2)
        );
    }

    #[test]
    fn spec_devcontainer_id_independent_of_insertion_order() {
        let mut l1 = BTreeMap::new();
        l1.insert("a".to_string(), "1".to_string());
        l1.insert("b".to_string(), "2".to_string());
        let mut l2 = BTreeMap::new();
        l2.insert("b".to_string(), "2".to_string());
        l2.insert("a".to_string(), "1".to_string());
        assert_eq!(spec_devcontainer_id(&l1), spec_devcontainer_id(&l2));
    }

    #[test]
    fn spec_empty_labels_produce_valid_id() {
        let labels = BTreeMap::new();
        let id = spec_devcontainer_id(&labels);
        assert_eq!(id.len(), 52);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn resolved_config_exposes_devcontainer_id() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);
        let resolved = config(tmp.path(), None).unwrap();
        assert_eq!(resolved.devcontainer_id.len(), 52);
        assert!(
            resolved
                .devcontainer_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
        );
    }

    #[test]
    fn resolve_container_workspace_folder_substituted_in_mount() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(
            tmp.path(),
            r#"{
                "image": "ubuntu",
                "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}",
                "mounts": ["source=data,target=${containerWorkspaceFolder}/data,type=volume"]
            }"#,
        );
        let resolved = config(tmp.path(), None).unwrap();
        let mount = resolved.config["mounts"][0].as_str().unwrap();
        let basename = tmp
            .path()
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let expected_target = format!("/workspaces/{basename}/data");
        assert!(
            mount.contains(&expected_target),
            "mount should contain substituted containerWorkspaceFolder, got: {mount}"
        );
    }

    #[test]
    fn typed_populated_for_valid_config() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu", "name": "test"}"#);
        let resolved = config(tmp.path(), None).unwrap();
        assert!(resolved.typed.is_some());
        assert_eq!(resolved.typed.as_ref().unwrap().name(), Some("test"));
    }

    #[test]
    fn typed_populated_with_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu", "unknownField": true}"#);
        let resolved = config(tmp.path(), None).unwrap();
        assert!(resolved.typed.is_some());
    }
}
