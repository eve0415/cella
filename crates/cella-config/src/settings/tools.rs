use serde::Deserialize;

use super::{ClaudeCode, Codex, Gemini, Nvim, Tmux};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    /// Tools to eagerly install during `cella up`.
    ///
    /// Valid values: `"claude-code"`, `"codex"`, `"gemini"`, `"nvim"`, `"tmux"`.
    #[serde(default)]
    pub install: Vec<String>,

    #[serde(default, rename = "claude-code")]
    pub claude_code: ClaudeCode,

    #[serde(default)]
    pub codex: Codex,

    #[serde(default)]
    pub gemini: Gemini,

    #[serde(default)]
    pub nvim: Nvim,

    #[serde(default)]
    pub tmux: Tmux,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_case_claude_code_key() {
        let toml_str = r#"
[claude-code]
version = "1.0.58"
"#;
        let tools: Tools = toml::from_str(toml_str).unwrap();
        assert_eq!(tools.claude_code.version, "1.0.58");
        assert!(tools.codex.forward_config);
    }

    #[test]
    fn partial_tools_config() {
        let toml_str = r#"
[codex]
forward_config = false
version = "0.1.2"
"#;
        let tools: Tools = toml::from_str(toml_str).unwrap();
        assert!(!tools.codex.forward_config);
        assert_eq!(tools.codex.version, "0.1.2");
        assert!(tools.claude_code.forward_config);
        assert!(tools.gemini.forward_config);
    }

    #[test]
    fn unknown_tool_rejected() {
        let result = toml::from_str::<Tools>("[unknown_tool]\nforward_config = true\n");
        assert!(result.is_err());
    }

    #[test]
    fn install_list_defaults_empty() {
        let tools: Tools = toml::from_str("").unwrap();
        assert!(tools.install.is_empty());
    }

    #[test]
    fn install_list_parses() {
        let tools: Tools = toml::from_str(r#"install = ["claude-code", "nvim", "tmux"]"#).unwrap();
        assert_eq!(tools.install, vec!["claude-code", "nvim", "tmux"]);
    }

    #[test]
    fn install_list_with_tool_config() {
        let toml_str = r#"
install = ["claude-code"]

[claude-code]
version = "1.0.58"
"#;
        let tools: Tools = toml::from_str(toml_str).unwrap();
        assert_eq!(tools.install, vec!["claude-code"]);
        assert_eq!(tools.claude_code.version, "1.0.58");
    }
}
