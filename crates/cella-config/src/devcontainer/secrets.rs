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
    /// Returns `Some` when `value` is a JSON object whose `description` and
    /// `documentationUrl` fields, if present, are strings. Returns `None` when
    /// the value is not an object or any known field has the wrong type —
    /// callers should treat such entries as malformed and skip them.
    ///
    /// Unknown fields are silently ignored for forward compatibility.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        // The schema requires each secret entry to be a JSON object.
        if !value.is_object() {
            return None;
        }

        // If a known field is present but has the wrong type, reject the entry.
        let description = match value.get("description") {
            Some(v) => Some(v.as_str().map(str::to_owned)?),
            None => None,
        };
        let documentation_url = match value.get("documentationUrl") {
            Some(v) => Some(v.as_str().map(str::to_owned)?),
            None => None,
        };

        Some(Self {
            description,
            documentation_url,
        })
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
        let decl = SecretDeclaration::from_value(&value).expect("valid object");
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
        let decl = SecretDeclaration::from_value(&value).expect("valid object");
        assert_eq!(decl.description.as_deref(), Some("An API key"));
        assert!(decl.documentation_url.is_none());
    }

    #[test]
    fn parse_documentation_url_only() {
        let value = serde_json::json!({ "documentationUrl": "https://example.com/secret" });
        let decl = SecretDeclaration::from_value(&value).expect("valid object");
        assert!(decl.description.is_none());
        assert_eq!(
            decl.documentation_url.as_deref(),
            Some("https://example.com/secret")
        );
    }

    #[test]
    fn parse_empty_object() {
        let value = serde_json::json!({});
        let decl = SecretDeclaration::from_value(&value).expect("valid object");
        assert!(decl.description.is_none());
        assert!(decl.documentation_url.is_none());
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let value = serde_json::json!({
            "description": "A secret",
            "unknownField": true
        });
        let decl = SecretDeclaration::from_value(&value).expect("valid object");
        assert_eq!(decl.description.as_deref(), Some("A secret"));
    }

    // Regression: malformed entries (non-object value) must be rejected, not
    // silently coerced into an empty SecretDeclaration.
    #[test]
    fn rejects_string_value() {
        let value = serde_json::json!("abc");
        assert!(SecretDeclaration::from_value(&value).is_none());
    }

    #[test]
    fn rejects_integer_value() {
        let value = serde_json::json!(42);
        assert!(SecretDeclaration::from_value(&value).is_none());
    }

    #[test]
    fn rejects_boolean_value() {
        let value = serde_json::json!(true);
        assert!(SecretDeclaration::from_value(&value).is_none());
    }

    #[test]
    fn rejects_array_value() {
        let value = serde_json::json!(["a", "b"]);
        assert!(SecretDeclaration::from_value(&value).is_none());
    }

    // Regression: a typo like `documentationURL` (wrong case) is unknown and
    // ignored; the known field `documentationUrl` stays None.
    #[test]
    fn ignores_typo_field_name() {
        let value = serde_json::json!({ "documentationURL": "https://example.com" });
        let decl = SecretDeclaration::from_value(&value).expect("valid object");
        assert!(decl.documentation_url.is_none());
    }

    // Regression: known field present with wrong type must be rejected.
    #[test]
    fn rejects_non_string_description() {
        let value = serde_json::json!({ "description": 123 });
        assert!(SecretDeclaration::from_value(&value).is_none());
    }

    #[test]
    fn rejects_non_string_documentation_url() {
        let value = serde_json::json!({ "documentationUrl": true });
        assert!(SecretDeclaration::from_value(&value).is_none());
    }
}
