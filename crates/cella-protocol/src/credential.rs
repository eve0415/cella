//! Git credential protocol field parsing and formatting.
//!
//! Shared between the in-container agent and host daemon for
//! encoding/decoding git credential helper key=value lines.

use std::collections::HashMap;

/// Parse git credential protocol fields from key=value text.
///
/// Each non-empty line is expected to contain `key=value`.
/// Lines without `=` are silently skipped.
pub fn parse_credential_fields(data: &str) -> HashMap<String, String> {
    data.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Format credential fields as key=value lines terminated by a blank line.
///
/// Produces output compatible with the git credential helper protocol.
pub fn format_credential_fields<S: std::hash::BuildHasher>(
    fields: &HashMap<String, String, S>,
) -> String {
    let mut output = String::new();
    for (key, value) in fields {
        output.push_str(key);
        output.push('=');
        output.push_str(value);
        output.push('\n');
    }
    output.push('\n');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fields() {
        let input = "protocol=https\nhost=github.com\n\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("protocol"), Some(&"https".to_string()));
        assert_eq!(fields.get("host"), Some(&"github.com".to_string()));
    }

    #[test]
    fn parse_empty() {
        let fields = parse_credential_fields("");
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_with_username() {
        let input = "protocol=https\nhost=github.com\nusername=user\n";
        let fields = parse_credential_fields(input);
        assert_eq!(fields.get("username"), Some(&"user".to_string()));
    }

    #[test]
    fn format_fields() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        let output = format_credential_fields(&fields);
        assert!(output.contains("protocol=https\n"));
        assert!(output.ends_with("\n\n"));
    }

    #[test]
    fn roundtrip() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        fields.insert("host".to_string(), "github.com".to_string());
        let formatted = format_credential_fields(&fields);
        let parsed = parse_credential_fields(&formatted);
        assert_eq!(parsed, fields);
    }
}
