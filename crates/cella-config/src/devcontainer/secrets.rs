/// A declared secret entry from the top-level `secrets` property in devcontainer.json.
///
/// Per the devcontainer spec, `secrets` is advisory metadata — it documents
/// what environment variables a container expects to have set, but does not
/// provide or inject their values. Value injection is handled separately via
/// the `--secrets-file` CLI flag.
///
/// Reference: <https://containers.dev/implementors/json_reference/#general-properties>
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretDeclaration {
    /// A human-readable description of what this secret is for.
    pub description: Option<String>,
    /// A URL to documentation about the secret.
    pub documentation_url: Option<String>,
}

impl SecretDeclaration {
    /// Parse a `SecretDeclaration` from a `serde_json::Value`.
    ///
    /// The value must be a JSON object. Unknown fields are ignored to match
    /// the `additionalProperties: false` schema (which only has `description`
    /// and `documentationUrl`).
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        let description = value
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let documentation_url = value
            .get("documentationUrl")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        Self {
            description,
            documentation_url,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_declaration() {
        let value = serde_json::json!({
            "description": "GitHub token for API access",
            "documentationUrl": "https://docs.github.com/en/authentication"
        });
        let decl = SecretDeclaration::from_value(&value);
        assert_eq!(
            decl.description.as_deref(),
            Some("GitHub token for API access")
        );
        assert_eq!(
            decl.documentation_url.as_deref(),
            Some("https://docs.github.com/en/authentication")
        );
    }

    #[test]
    fn parse_description_only() {
        let value = serde_json::json!({ "description": "An API key" });
        let decl = SecretDeclaration::from_value(&value);
        assert_eq!(decl.description.as_deref(), Some("An API key"));
        assert!(decl.documentation_url.is_none());
    }

    #[test]
    fn parse_documentation_url_only() {
        let value = serde_json::json!({ "documentationUrl": "https://example.com/secret" });
        let decl = SecretDeclaration::from_value(&value);
        assert!(decl.description.is_none());
        assert_eq!(
            decl.documentation_url.as_deref(),
            Some("https://example.com/secret")
        );
    }

    #[test]
    fn parse_empty_object() {
        let value = serde_json::json!({});
        let decl = SecretDeclaration::from_value(&value);
        assert!(decl.description.is_none());
        assert!(decl.documentation_url.is_none());
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let value = serde_json::json!({
            "description": "A secret",
            "unknownField": true
        });
        let decl = SecretDeclaration::from_value(&value);
        assert_eq!(decl.description.as_deref(), Some("A secret"));
    }
}
