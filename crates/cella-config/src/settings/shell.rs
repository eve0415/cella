use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Shell {
    #[serde(default)]
    pub preferred: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_empty_preferred() {
        let shell = Shell::default();
        assert!(shell.preferred.is_empty());
    }

    #[test]
    fn deserialize_preferred_list() {
        let shell: Shell = toml::from_str(r#"preferred = ["zsh", "bash", "sh"]"#).unwrap();
        assert_eq!(shell.preferred, vec!["zsh", "bash", "sh"]);
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let shell: Shell = toml::from_str("").unwrap();
        assert!(shell.preferred.is_empty());
    }

    #[test]
    fn deserialize_full_paths() {
        let shell: Shell =
            toml::from_str(r#"preferred = ["/usr/local/bin/fish", "zsh", "bash", "sh"]"#).unwrap();
        assert_eq!(shell.preferred[0], "/usr/local/bin/fish");
    }

    #[test]
    fn unknown_field_rejected() {
        let result = toml::from_str::<Shell>("unknown = true");
        assert!(result.is_err());
    }
}
