use serde::Deserialize;

const fn default_true() -> bool {
    true
}

/// Credential forwarding settings.
#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    /// Forward gh CLI credentials into containers (default: true).
    #[serde(default = "default_true")]
    pub gh: bool,
    // Future: claude, codex, gemini
}

impl Default for Credentials {
    fn default() -> Self {
        Self { gh: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_gh() {
        let settings = Credentials::default();
        assert!(settings.gh);
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Credentials = toml::from_str("").unwrap();
        assert!(settings.gh);
    }

    #[test]
    fn deserialize_explicit_false() {
        let settings: Credentials = toml::from_str("gh = false").unwrap();
        assert!(!settings.gh);
    }

    #[test]
    fn deserialize_explicit_true() {
        let settings: Credentials = toml::from_str("gh = true").unwrap();
        assert!(settings.gh);
    }
}
