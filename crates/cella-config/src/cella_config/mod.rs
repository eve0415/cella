pub mod cli;
pub mod error;
mod format;
mod merge;
pub mod security;

use std::path::Path;

use serde::Deserialize;

pub use cli::{Cli, CliBuild, OutputFormat, PullPolicy};
pub use error::CellaConfigError;
pub use security::{Security, SecurityMode};

use crate::settings::{Credentials, Network, Tools};

use self::format::load_layer;
use self::merge::merge_layers;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellaConfig {
    #[serde(default)]
    pub security: Security,

    #[serde(default)]
    pub credentials: Credentials,

    #[serde(default)]
    pub tools: Tools,

    #[serde(default)]
    pub network: Network,

    #[serde(default)]
    pub cli: Cli,
}

impl CellaConfig {
    /// Load cella config by merging global, customizations.cella, and project layers.
    ///
    /// # Errors
    ///
    /// Returns `CellaConfigError` if any config file is malformed or contains unknown fields.
    pub fn load(
        workspace: &Path,
        resolved: Option<&crate::devcontainer::resolve::ResolvedConfig>,
    ) -> Result<Self, CellaConfigError> {
        Self::load_with_global(workspace, resolved, None)
    }

    /// Load with an explicit global config directory (for test isolation).
    ///
    /// # Errors
    ///
    /// Returns `CellaConfigError` if any config file is malformed or contains unknown fields.
    pub fn load_with_global(
        workspace: &Path,
        resolved: Option<&crate::devcontainer::resolve::ResolvedConfig>,
        global_dir: Option<&Path>,
    ) -> Result<Self, CellaConfigError> {
        let mut layers = Vec::new();

        let global = match global_dir {
            Some(dir) => load_layer(dir, "config")?,
            None => {
                if let Some(dir) = cella_env::paths::cella_data_dir() {
                    load_layer(&dir, "config")?
                } else {
                    None
                }
            }
        };
        if let Some(val) = global {
            layers.push(val);
        }

        if let Some(rc) = resolved
            && let Some(cella_customizations) =
                rc.config.get("customizations").and_then(|c| c.get("cella"))
        {
            layers.push(cella_customizations.clone());
        }

        let project_dir = workspace.join(".devcontainer");
        if let Some(val) = load_layer(&project_dir, "cella")? {
            layers.push(val);
        }

        let merged = merge_layers(&layers);
        serde_json::from_value(merged).map_err(|e| CellaConfigError::Deserialization { source: e })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    fn make_resolved(config: Value) -> crate::devcontainer::resolve::ResolvedConfig {
        let typed = crate::schema::DevContainer::validate(&config, "").ok();
        crate::devcontainer::resolve::ResolvedConfig {
            config,
            config_path: std::path::PathBuf::new(),
            workspace_root: std::path::PathBuf::new(),
            config_hash: String::new(),
            warnings: vec![],
            typed,
        }
    }

    #[test]
    fn no_config_files_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let cfg = CellaConfig::load_with_global(tmp.path(), None, Some(tmp.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Disabled);
        assert!(cfg.credentials.gh);
        assert!(cfg.tools.claude_code.enabled);
    }

    #[test]
    fn global_toml_loaded() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
    }

    #[test]
    fn project_overrides_global() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"logged\"\n\n[credentials]\ngh = true\n",
        )
        .unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
        assert!(cfg.credentials.gh);
    }

    #[test]
    fn customizations_cella_extracted() {
        let ws = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let resolved = make_resolved(json!({
            "customizations": {
                "cella": {
                    "security": {"mode": "logged"}
                }
            }
        }));
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Logged);
    }

    #[test]
    fn three_layer_precedence() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"disabled\"\n",
        )
        .unwrap();
        let resolved = make_resolved(json!({
            "customizations": {
                "cella": {
                    "security": {"mode": "logged"}
                }
            }
        }));
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
    }

    #[test]
    fn invalid_toml_hard_fails() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(global.path().join("config.toml"), "not valid {{{").unwrap();
        let err = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap_err();
        assert!(matches!(err, CellaConfigError::ParseToml { .. }));
    }

    #[test]
    fn deny_unknown_fields_rejects_typos() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[securityy]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let err = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap_err();
        assert!(matches!(err, CellaConfigError::Deserialization { .. }));
    }

    #[test]
    fn toml_preferred_over_json() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        std::fs::write(
            global.path().join("config.json"),
            r#"{"security": {"mode": "logged"}}"#,
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Enforced);
    }

    #[test]
    fn jsonc_comments_in_json() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.json"),
            r#"{
                // comment
                "security": {
                    "mode": "logged" /* inline */
                }
            }"#,
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Logged);
    }

    #[test]
    fn missing_customizations_skipped() {
        let ws = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let resolved = make_resolved(json!({"image": "ubuntu"}));
        let cfg =
            CellaConfig::load_with_global(ws.path(), Some(&resolved), Some(global.path())).unwrap();
        assert_eq!(cfg.security.mode, SecurityMode::Disabled);
    }

    #[test]
    fn deep_merge_preserves_sibling_fields() {
        let global = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(
            global.path().join("config.toml"),
            "[credentials]\ngh = false\n\n[credentials.ai]\nenabled = true\nopenai = false\n",
        )
        .unwrap();
        let devcontainer = ws.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer).unwrap();
        std::fs::write(
            devcontainer.join("cella.toml"),
            "[credentials.ai]\nanthropic = false\n",
        )
        .unwrap();
        let cfg = CellaConfig::load_with_global(ws.path(), None, Some(global.path())).unwrap();
        assert!(!cfg.credentials.gh);
        assert!(cfg.credentials.ai.enabled);
        assert!(!cfg.credentials.ai.is_provider_enabled("openai"));
        assert!(!cfg.credentials.ai.is_provider_enabled("anthropic"));
    }
}
