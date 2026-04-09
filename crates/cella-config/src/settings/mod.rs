mod ai_credentials;
mod claude_code;
mod codex;
mod credentials;
mod gemini;
pub mod network;
mod nvim;
mod tmux;

use std::path::Path;

use serde::Deserialize;
use tracing::debug;

pub use ai_credentials::AiCredentials;
pub use claude_code::ClaudeCode;
pub use codex::Codex;
pub use credentials::Credentials;
pub use gemini::Gemini;
pub use network::Network;
pub use nvim::Nvim;
pub use tmux::Tmux;

/// Tool installation and forwarding settings.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Tools {
    /// Claude Code settings.
    #[serde(default, rename = "claude-code")]
    pub claude_code: ClaudeCode,

    /// `OpenAI` Codex CLI settings.
    #[serde(default)]
    pub codex: Codex,

    /// Google Gemini CLI settings.
    #[serde(default)]
    pub gemini: Gemini,

    /// Neovim settings.
    #[serde(default)]
    pub nvim: Nvim,

    /// Tmux settings.
    #[serde(default)]
    pub tmux: Tmux,
}

/// Cella's own settings, loaded from TOML config files.
///
/// Global config: `~/.cella/config.toml`
/// Project config: `<workspace>/.devcontainer/cella.toml`
///
/// Project settings override global settings per-key.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Settings {
    /// Credential forwarding settings.
    #[serde(default)]
    pub credentials: Credentials,

    /// Tool installation and forwarding settings.
    #[serde(default)]
    pub tools: Tools,

    /// Network proxy and blocking settings.
    #[serde(default)]
    pub network: Network,
}

impl Settings {
    /// Load settings by merging global (`~/.cella/config.toml`) and
    /// project (`.devcontainer/cella.toml`) config files.
    ///
    /// Missing files are silently ignored. Parse errors log a warning
    /// and fall back to defaults.
    pub fn load(workspace_root: &Path) -> Self {
        let global = Self::load_global();
        let project = Self::load_project(workspace_root);

        Self::merge(global, project)
    }

    fn load_global() -> Option<Self> {
        let home = std::env::var("HOME").ok()?;
        let path = Path::new(&home).join(".cella").join("config.toml");
        Self::load_file(&path)
    }

    fn load_project(workspace_root: &Path) -> Option<Self> {
        let path = workspace_root.join(".devcontainer").join("cella.toml");
        Self::load_file(&path)
    }

    fn load_file(path: &Path) -> Option<Self> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return None;
        };
        debug!("Loading cella settings from {}", path.display());
        match toml::from_str(&content) {
            Ok(settings) => Some(settings),
            Err(e) => {
                tracing::warn!("Failed to parse {}: {e}", path.display());
                None
            }
        }
    }

    /// Merge global and project settings. Project overrides global per-section.
    ///
    /// Destructuring both sides forces a compile error when new fields are added
    /// to `Settings` or `Tools` without updating this method.
    fn merge(global: Option<Self>, project: Option<Self>) -> Self {
        match (global, project) {
            (None, None) => Self::default(),
            (Some(g), None) => g,
            (None, Some(p)) => p,
            (Some(_g), Some(p)) => {
                let Self {
                    credentials: pc,
                    tools: pt,
                    network: pn,
                } = p;
                let Tools {
                    claude_code: pt_claude,
                    codex: pt_codex,
                    gemini: pt_gemini,
                    nvim: pt_nvim,
                    tmux: pt_tmux,
                } = pt;
                Self {
                    credentials: pc,
                    tools: Tools {
                        claude_code: pt_claude,
                        codex: pt_codex,
                        gemini: pt_gemini,
                        nvim: pt_nvim,
                        tmux: pt_tmux,
                    },
                    network: pn,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_settings() {
        let settings = Settings::default();
        assert!(settings.credentials.gh);
        assert!(settings.credentials.ai.enabled);
        assert!(settings.tools.claude_code.enabled);
        assert!(settings.tools.claude_code.forward_config);
        assert_eq!(settings.tools.claude_code.version, "latest");
        assert!(settings.tools.codex.enabled);
        assert!(settings.tools.codex.forward_config);
        assert_eq!(settings.tools.codex.version, "latest");
        assert!(settings.tools.gemini.enabled);
        assert!(settings.tools.gemini.forward_config);
        assert_eq!(settings.tools.gemini.version, "latest");
        assert!(settings.tools.nvim.forward_config);
        assert_eq!(settings.tools.nvim.version, "stable");
        assert!(settings.tools.nvim.config_path.is_none());
        assert!(settings.tools.tmux.forward_config);
        assert!(settings.tools.tmux.config_path.is_none());
    }

    #[test]
    fn load_missing_files_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let settings = Settings::load(tmp.path());
        assert!(settings.credentials.gh);
    }

    #[test]
    fn load_project_config() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cella.toml"), "[credentials]\ngh = false\n").unwrap();

        let settings = Settings::load(tmp.path());
        assert!(!settings.credentials.gh);
    }

    #[test]
    fn load_empty_toml_uses_defaults() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cella.toml"), "").unwrap();

        let settings = Settings::load(tmp.path());
        assert!(settings.credentials.gh);
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
[credentials]
gh = false

[credentials.ai]
enabled = true
openai = false

[tools.claude-code]
enabled = false
version = "stable"

[tools.codex]
enabled = false
version = "0.1.2"

[tools.gemini]
enabled = false
version = "0.5.0"

[tools.nvim]
forward_config = false
version = "0.10.3"
config_path = "~/dotfiles/nvim"

[tools.tmux]
forward_config = false
config_path = "~/dotfiles/.tmux.conf"
"#;
        let settings: Settings = toml::from_str(toml_str).unwrap();
        assert!(!settings.credentials.gh);
        assert!(settings.credentials.ai.enabled);
        assert!(!settings.credentials.ai.is_provider_enabled("openai"));
        assert!(settings.credentials.ai.is_provider_enabled("anthropic"));
        assert!(!settings.tools.claude_code.enabled);
        assert_eq!(settings.tools.claude_code.version, "stable");
        assert!(!settings.tools.codex.enabled);
        assert_eq!(settings.tools.codex.version, "0.1.2");
        assert!(!settings.tools.gemini.enabled);
        assert_eq!(settings.tools.gemini.version, "0.5.0");
        assert!(!settings.tools.nvim.forward_config);
        assert_eq!(settings.tools.nvim.version, "0.10.3");
        assert_eq!(
            settings.tools.nvim.config_path.as_deref(),
            Some("~/dotfiles/nvim")
        );
        assert!(!settings.tools.tmux.forward_config);
        assert_eq!(
            settings.tools.tmux.config_path.as_deref(),
            Some("~/dotfiles/.tmux.conf")
        );
    }

    #[test]
    fn parse_tools_only() {
        let toml_str = r#"
[tools.claude-code]
enabled = false
forward_config = false
version = "1.0.58"
"#;
        let settings: Settings = toml::from_str(toml_str).unwrap();
        assert!(!settings.tools.claude_code.enabled);
        assert!(!settings.tools.claude_code.forward_config);
        assert_eq!(settings.tools.claude_code.version, "1.0.58");
        // credentials and other tools should still be default
        assert!(settings.credentials.gh);
        assert!(settings.tools.codex.enabled);
        assert!(settings.tools.gemini.enabled);
    }

    #[test]
    fn parse_codex_only() {
        let toml_str = r#"
[tools.codex]
enabled = false
forward_config = false
version = "0.1.2"
"#;
        let settings: Settings = toml::from_str(toml_str).unwrap();
        assert!(!settings.tools.codex.enabled);
        assert!(!settings.tools.codex.forward_config);
        assert_eq!(settings.tools.codex.version, "0.1.2");
        // other tools default
        assert!(settings.tools.claude_code.enabled);
        assert!(settings.tools.gemini.enabled);
    }

    #[test]
    fn parse_gemini_only() {
        let toml_str = r#"
[tools.gemini]
enabled = false
forward_config = false
version = "0.5.0"
"#;
        let settings: Settings = toml::from_str(toml_str).unwrap();
        assert!(!settings.tools.gemini.enabled);
        assert!(!settings.tools.gemini.forward_config);
        assert_eq!(settings.tools.gemini.version, "0.5.0");
        // other tools default
        assert!(settings.tools.claude_code.enabled);
        assert!(settings.tools.codex.enabled);
    }

    #[test]
    fn merge_project_overrides_global() {
        let global = Settings {
            credentials: Credentials {
                gh: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let project = Settings {
            credentials: Credentials {
                gh: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = Settings::merge(Some(global), Some(project));
        assert!(!merged.credentials.gh);
    }

    #[test]
    fn merge_global_only() {
        let global = Settings {
            credentials: Credentials {
                gh: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = Settings::merge(Some(global), None);
        assert!(!merged.credentials.gh);
    }

    #[test]
    fn merge_neither() {
        let merged = Settings::merge(None, None);
        assert!(merged.credentials.gh);
    }

    #[test]
    fn invalid_toml_falls_back_to_none() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cella.toml"), "not valid toml {{{").unwrap();

        // Should not panic, just return defaults
        let settings = Settings::load(tmp.path());
        assert!(settings.credentials.gh);
    }
}
