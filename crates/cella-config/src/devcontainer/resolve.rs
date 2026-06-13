//! Config resolution: discover, parse, merge layers, compute hash.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cella_backend::names::lexical_absolute;
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
    /// The raw (pre-substitution) `remoteEnv` object from the merged config.
    ///
    /// Captured before the phase-1 substitution pass so the orchestrator can
    /// perform a second-pass resolution after `userEnvProbe` has captured the
    /// container's live environment, enabling `${containerEnv:VAR}` in
    /// `remoteEnv` to resolve against the actual running container.  `None`
    /// when the merged config has no `remoteEnv` key.
    pub raw_remote_env: Option<serde_json::Value>,
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

    /// Returns the declared secrets from the top-level `secrets` property.
    ///
    /// Each entry names an environment variable that the container expects to
    /// have set, along with optional documentation metadata. These are
    /// advisory — they do not inject values. Use `--secrets-file` for
    /// value injection.
    ///
    /// Returns `None` when the config has no `secrets` key, or an empty map
    /// when the key is present but empty.
    #[must_use]
    pub fn secrets(
        &self,
    ) -> Option<std::collections::HashMap<String, super::secrets::SecretDeclaration>> {
        let raw_map = self.typed.as_ref()?.secrets()?;
        Some(
            raw_map
                .iter()
                // filter_map drops entries whose value does not conform to the schema
                // (non-object or known field with wrong type). Valid but empty objects
                // {} are retained as SecretDeclaration { description: None, documentation_url: None }.
                .filter_map(|(k, v)| {
                    super::secrets::SecretDeclaration::from_value(v).map(|decl| (k.clone(), decl))
                })
                .collect(),
        )
    }
}

/// Compute the spec-compliant `devcontainerId`.
///
/// Per <https://containers.dev/implementors/spec/#devcontainerid>:
/// SHA-256 of the sorted JSON label object, base-32 encoded, left-padded to 52 chars.
pub fn devcontainer_id(workspace_root: &Path, config_path: &Path) -> String {
    // Match the official CLI's label values, which use a LEXICAL absolute path
    // (Node's `path.resolve`) — it collapses `.`/`..` but never follows
    // symlinks. Using `canonicalize()` here would resolve symlinks (e.g. macOS
    // `/tmp` -> `/private/tmp`, bind mounts) and produce a different id than VS
    // Code / the official CLI, breaking cross-tool container reuse and shared
    // volume names.
    let mut labels = BTreeMap::new();
    labels.insert(
        "devcontainer.local_folder".to_string(),
        lexical_absolute(workspace_root)
            .to_string_lossy()
            .to_string(),
    );
    labels.insert(
        "devcontainer.config_file".to_string(),
        lexical_absolute(config_path).to_string_lossy().to_string(),
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
    config_with_override(workspace_root, config_path_override, None)
}

/// Resolve config with `--override-config` semantics.
///
/// `override_config_file` supplies the config *contents* (fully replacing any
/// discovered devcontainer.json), while the recorded `config_path` stays the
/// `--config` path or the discovered/default path so labels and the config hash
/// key off the workspace's canonical location. When set, the global and
/// workspace-local layer merges are bypassed for single-document parity with
/// the official CLI.
///
/// # Errors
///
/// Returns an error if the config file cannot be read or parsed.
pub fn config_with_override(
    workspace_root: &Path,
    config_path_override: Option<&Path>,
    override_config_file: Option<&Path>,
) -> Result<ResolvedConfig, CellaConfigError> {
    let config_path = if let Some(override_path) = config_path_override {
        override_path.to_path_buf()
    } else if override_config_file.is_some() {
        discover::config(workspace_root)
            .unwrap_or_else(|_| workspace_root.join(".devcontainer/devcontainer.json"))
    } else {
        discover::config(workspace_root)?
    };

    // Content comes from the override file when provided, else from config_path.
    let read_path = override_config_file.unwrap_or(config_path.as_path());
    let raw_text =
        std::fs::read_to_string(read_path).map_err(|source| CellaConfigError::ReadFile {
            path: read_path.display().to_string(),
            source,
        })?;

    // Parse for validation warnings (non-strict mode)
    let warnings = match parse::devcontainer(&read_path.display().to_string(), &raw_text, false) {
        Ok((_, warnings)) => warnings,
        Err(diags) => diags.diagnostics().to_vec(),
    };

    // Get raw JSON Value for merging (strip JSONC, parse to Value)
    let cleaned = jsonc::strip(&raw_text).map_err(|e| CellaConfigError::Jsonc(e.to_string()))?;
    let mut config: serde_json::Value = serde_json::from_str(&cleaned)?;

    // --override-config means "this exact document": skip cella's global and
    // workspace-local layer merges to match the official single-read semantics.
    if override_config_file.is_none() {
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

    // Snapshot raw remoteEnv BEFORE phase-1 substitution so the orchestrator
    // can perform a second pass after userEnvProbe, resolving
    // ${containerEnv:VAR} against the live container environment.
    let raw_remote_env = config.get("remoteEnv").cloned();

    ctx.substitute_value(&mut config);

    // Deprecation warnings for legacy properties
    let mut warnings = warnings;
    if config.get("appPort").is_some() {
        warnings.push(Diagnostic {
            severity: Severity::Warning,
            message: "\"appPort\" is deprecated. Use \"forwardPorts\" instead.".into(),
            path: "$.appPort".into(),
            span: None,
            help: Some(
                "Replace \"appPort\" with \"forwardPorts\" in your devcontainer.json".into(),
            ),
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
        raw_remote_env,
    })
}

/// Build a [`ResolvedConfig`] directly from an in-memory config `Value`,
/// without reading or discovering any file.
///
/// Used by the `up` no-workspace path (`--id-label` with no `--override-config`
/// and no cwd devcontainer.json) where the config is sourced from a found
/// container's `devcontainer.metadata` label rather than the filesystem. The
/// config hash, `devcontainerId`, and typed view are computed the same way as
/// [`config_with_override`]; `workspace_root` is the nominal cwd and
/// `config_path` the recorded (not-necessarily-existing) path.
#[must_use]
pub fn from_config_value(
    config: serde_json::Value,
    workspace_root: &Path,
    config_path: PathBuf,
) -> ResolvedConfig {
    let devcontainer_id = devcontainer_id(workspace_root, &config_path);
    let canonical = serde_json::to_string(&config).unwrap_or_default();
    let hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    let typed = crate::schema::DevContainer::validate(&config, "").ok();

    ResolvedConfig {
        config,
        config_path,
        workspace_root: workspace_root.to_path_buf(),
        config_hash: hash,
        devcontainer_id,
        warnings: Vec::new(),
        typed,
        // Config sourced from a container label is already substituted — no
        // raw snapshot is available for a second-pass containerEnv resolution.
        raw_remote_env: None,
    }
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
    use cella_backend::names::lexical_absolute;
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
    fn from_config_value_computes_hash_and_id() {
        let cfg = serde_json::json!({"image": "ubuntu", "workspaceMount": ""});
        let resolved = from_config_value(
            cfg,
            Path::new("/cwd"),
            PathBuf::from("/cwd/.devcontainer/devcontainer.json"),
        );
        assert_eq!(resolved.config["image"], "ubuntu");
        assert_eq!(resolved.config["workspaceMount"], "");
        assert!(!resolved.config_hash.is_empty());
        assert_eq!(resolved.config_hash.len(), 64, "sha256 hex is 64 chars");
        assert_eq!(resolved.devcontainer_id.len(), 52);
        assert_eq!(resolved.workspace_root, Path::new("/cwd"));
        assert!(
            resolved.typed.is_some(),
            "valid config parses to typed view"
        );
    }

    #[test]
    fn from_config_value_hash_matches_file_resolution() {
        // Same content via file-discovery and via from_config_value must hash
        // identically (canonical-JSON serialization, same algorithm).
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);
        let from_file = config(tmp.path(), None).unwrap();
        let from_value = from_config_value(
            serde_json::json!({"image": "ubuntu"}),
            tmp.path(),
            from_file.config_path.clone(),
        );
        assert_eq!(from_file.config_hash, from_value.config_hash);
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
    fn override_config_replaces_content_but_records_default_path() {
        let tmp = TempDir::new().unwrap();
        // A workspace devcontainer.json that should be IGNORED for content.
        create_devcontainer(tmp.path(), r#"{"image": "workspace-image"}"#);

        let override_file = tmp.path().join("override.json");
        std::fs::write(&override_file, r#"{"image": "override-image"}"#).unwrap();

        let resolved =
            config_with_override(tmp.path(), None, Some(override_file.as_path())).unwrap();

        // Content comes from the override file.
        assert_eq!(resolved.config["image"], "override-image");
        // Recorded path is the discovered/default workspace config, NOT the
        // override file — so labels/hash key off the canonical location.
        assert!(
            resolved
                .config_path
                .ends_with(".devcontainer/devcontainer.json")
        );
        assert_ne!(resolved.config_path, override_file);
    }

    #[test]
    fn override_config_works_without_workspace_config() {
        let tmp = TempDir::new().unwrap();
        // No .devcontainer in the workspace at all.
        let override_file = tmp.path().join("over.json");
        std::fs::write(&override_file, r#"{"image": "only-override"}"#).unwrap();

        let resolved =
            config_with_override(tmp.path(), None, Some(override_file.as_path())).unwrap();
        assert_eq!(resolved.config["image"], "only-override");
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
    fn lexical_absolute_collapses_dot_dot_without_symlinks() {
        assert_eq!(lexical_absolute(Path::new("/a/b/../c")), Path::new("/a/c"));
        assert_eq!(lexical_absolute(Path::new("/a/./b")), Path::new("/a/b"));
        // Cannot ascend past the root.
        assert_eq!(lexical_absolute(Path::new("/../x")), Path::new("/x"));
    }

    #[cfg(unix)]
    #[test]
    fn devcontainer_id_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let real = TempDir::new().unwrap();
        create_devcontainer(real.path(), r#"{"image": "ubuntu"}"#);
        let real_cfg = real.path().join(".devcontainer/devcontainer.json");

        // A symlink pointing at the real workspace.
        let link_parent = TempDir::new().unwrap();
        let link = link_parent.path().join("link");
        symlink(real.path(), &link).unwrap();
        let link_cfg = link.join(".devcontainer/devcontainer.json");

        // The id must derive from the symlink PATH (lexical), not its target —
        // otherwise it wouldn't match VS Code / the official CLI, which never
        // resolve symlinks for the id labels.
        let via_link = devcontainer_id(&link, &link_cfg);
        let via_real = devcontainer_id(real.path(), &real_cfg);
        assert_ne!(via_link, via_real);
        // And it's stable for the same (symlink) path.
        assert_eq!(via_link, devcontainer_id(&link, &link_cfg));
    }

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

    #[test]
    fn secrets_returns_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(tmp.path(), r#"{"image": "ubuntu"}"#);
        let resolved = config(tmp.path(), None).unwrap();
        assert!(resolved.secrets().is_none());
    }

    #[test]
    fn secrets_parses_full_declaration() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(
            tmp.path(),
            r#"{
                "image": "ubuntu",
                "secrets": {
                    "GITHUB_TOKEN": {
                        "description": "GitHub personal access token",
                        "documentationUrl": "https://docs.github.com/en/authentication"
                    }
                }
            }"#,
        );
        let resolved = config(tmp.path(), None).unwrap();
        let secrets = resolved.secrets().expect("secrets should be present");
        assert_eq!(secrets.len(), 1);
        let decl = secrets
            .get("GITHUB_TOKEN")
            .expect("GITHUB_TOKEN should exist");
        assert_eq!(
            decl.description.as_deref(),
            Some("GitHub personal access token")
        );
        assert_eq!(
            decl.documentation_url.as_deref(),
            Some("https://docs.github.com/en/authentication")
        );
    }

    #[test]
    fn secrets_parses_declaration_without_optional_fields() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(
            tmp.path(),
            r#"{
                "image": "ubuntu",
                "secrets": {
                    "API_KEY": {}
                }
            }"#,
        );
        let resolved = config(tmp.path(), None).unwrap();
        let secrets = resolved.secrets().expect("secrets should be present");
        assert_eq!(secrets.len(), 1);
        let decl = secrets.get("API_KEY").expect("API_KEY should exist");
        assert!(decl.description.is_none());
        assert!(decl.documentation_url.is_none());
    }

    #[test]
    fn secrets_parses_multiple_declarations() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(
            tmp.path(),
            r#"{
                "image": "ubuntu",
                "secrets": {
                    "GITHUB_TOKEN": { "description": "GitHub token" },
                    "NPM_TOKEN": { "description": "npm registry token" },
                    "DB_PASSWORD": {}
                }
            }"#,
        );
        let resolved = config(tmp.path(), None).unwrap();
        let secrets = resolved.secrets().expect("secrets should be present");
        assert_eq!(secrets.len(), 3);
        assert!(secrets.contains_key("GITHUB_TOKEN"));
        assert!(secrets.contains_key("NPM_TOKEN"));
        assert!(secrets.contains_key("GITHUB_TOKEN"));
        assert!(secrets.contains_key("NPM_TOKEN"));
        assert!(secrets.contains_key("DB_PASSWORD"));
    }

    // Regression: secret entries with a non-object value (e.g. a plain string)
    // must be silently dropped rather than coerced into an empty declaration.
    #[test]
    fn secrets_skips_malformed_non_object_entries() {
        let tmp = TempDir::new().unwrap();
        create_devcontainer(
            tmp.path(),
            r#"{
                "image": "ubuntu",
                "secrets": {
                    "VALID_TOKEN": { "description": "A valid secret" },
                    "BAD_TOKEN": "plain-string-not-allowed"
                }
            }"#,
        );
        let resolved = config(tmp.path(), None).unwrap();
        let secrets = resolved.secrets().expect("secrets key is present");
        // Only the valid entry survives; the malformed one is dropped.
        assert_eq!(secrets.len(), 1);
        assert!(secrets.contains_key("VALID_TOKEN"));
        assert!(!secrets.contains_key("BAD_TOKEN"));
    }
}
