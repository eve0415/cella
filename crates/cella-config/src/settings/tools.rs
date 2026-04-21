use serde::Deserialize;

use super::{ClaudeCode, Codex, Gemini, Nvim, Tmux};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Tools {
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
