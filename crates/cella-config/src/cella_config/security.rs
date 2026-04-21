use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecurityMode {
    #[default]
    Disabled,
    Logged,
    Enforced,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    #[serde(default)]
    pub mode: SecurityMode,
}

impl std::fmt::Display for SecurityMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Logged => write!(f, "logged"),
            Self::Enforced => write!(f, "enforced"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        assert_eq!(SecurityMode::default(), SecurityMode::Disabled);
        assert_eq!(Security::default().mode, SecurityMode::Disabled);
    }

    #[test]
    fn deserialize_modes() {
        let cases = [
            (r#"{"mode":"disabled"}"#, SecurityMode::Disabled),
            (r#"{"mode":"logged"}"#, SecurityMode::Logged),
            (r#"{"mode":"enforced"}"#, SecurityMode::Enforced),
        ];
        for (json, expected) in cases {
            let sec: Security = serde_json::from_str(json).unwrap();
            assert_eq!(sec.mode, expected);
        }
    }

    #[test]
    fn empty_object_uses_default() {
        let sec: Security = serde_json::from_str("{}").unwrap();
        assert_eq!(sec.mode, SecurityMode::Disabled);
    }

    #[test]
    fn invalid_mode_rejected() {
        let result = serde_json::from_str::<Security>(r#"{"mode":"unknown"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_field_rejected() {
        let result = serde_json::from_str::<Security>(r#"{"mode":"disabled","extra":true}"#);
        assert!(result.is_err());
    }

    #[test]
    fn display_modes() {
        assert_eq!(SecurityMode::Disabled.to_string(), "disabled");
        assert_eq!(SecurityMode::Logged.to_string(), "logged");
        assert_eq!(SecurityMode::Enforced.to_string(), "enforced");
    }
}
