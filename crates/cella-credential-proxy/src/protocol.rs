//! Git credential wire protocol parsing and formatting.
//!
//! Wire format over Unix socket:
//! ```text
//! <operation>\n          # get, store, erase, or ping
//! key1=value1\n          # credential fields
//! key2=value2\n
//! \n                     # empty line terminates
//! ```

use std::collections::HashMap;

use crate::CellaCredentialProxyError;

/// A parsed credential request from the container.
#[derive(Debug, Clone)]
pub struct CredentialRequest {
    /// The git credential operation: get, store, erase, or ping.
    pub operation: String,
    /// Key-value fields (protocol, host, username, password, etc.).
    pub fields: HashMap<String, String>,
}

/// A credential response to send back to the container.
#[derive(Debug, Clone)]
pub struct CredentialResponse {
    /// Key-value fields (protocol, host, username, password, etc.).
    pub fields: HashMap<String, String>,
}

/// Parse a credential request from raw bytes.
///
/// Expected format:
/// ```text
/// get\n
/// protocol=https\n
/// host=github.com\n
/// \n
/// ```
///
/// # Errors
///
/// Returns `CellaCredentialProxyError::Protocol` if the request is empty or malformed.
pub fn parse_request(data: &str) -> Result<CredentialRequest, CellaCredentialProxyError> {
    let mut lines = data.lines();

    let operation = lines
        .next()
        .ok_or_else(|| CellaCredentialProxyError::Protocol {
            message: "empty request".to_string(),
        })?
        .trim()
        .to_string();

    if operation.is_empty() {
        return Err(CellaCredentialProxyError::Protocol {
            message: "empty operation".to_string(),
        });
    }

    let mut fields = HashMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.to_string(), value.to_string());
        }
    }

    Ok(CredentialRequest { operation, fields })
}

/// Format credential fields for the git credential protocol.
///
/// Output format:
/// ```text
/// key1=value1\n
/// key2=value2\n
/// \n
/// ```
pub fn format_response(response: &CredentialResponse) -> String {
    let mut output = String::new();
    for (key, value) in &response.fields {
        output.push_str(key);
        output.push('=');
        output.push_str(value);
        output.push('\n');
    }
    output.push('\n');
    output
}

/// Format credential fields for piping into `git credential` stdin.
pub fn format_fields_for_stdin<S: std::hash::BuildHasher>(
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

/// Parse credential response from `git credential` stdout.
pub fn parse_credential_output(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_request() {
        let data = "get\nprotocol=https\nhost=github.com\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "get");
        assert_eq!(req.fields.get("protocol"), Some(&"https".to_string()));
        assert_eq!(req.fields.get("host"), Some(&"github.com".to_string()));
    }

    #[test]
    fn parse_store_request() {
        let data = "store\nprotocol=https\nhost=github.com\nusername=user\npassword=token\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "store");
        assert_eq!(req.fields.len(), 4);
    }

    #[test]
    fn parse_ping_request() {
        let data = "ping\n\n";
        let req = parse_request(data).unwrap();
        assert_eq!(req.operation, "ping");
        assert!(req.fields.is_empty());
    }

    #[test]
    fn parse_empty_request_fails() {
        let result = parse_request("");
        assert!(result.is_err());
    }

    #[test]
    fn format_response_output() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        fields.insert("host".to_string(), "github.com".to_string());
        let response = CredentialResponse { fields };
        let output = format_response(&response);
        assert!(output.contains("protocol=https\n"));
        assert!(output.contains("host=github.com\n"));
        assert!(output.ends_with("\n\n"));
    }

    #[test]
    fn parse_credential_output_roundtrip() {
        let input = "protocol=https\nhost=github.com\nusername=user\npassword=ghp_xxx\n";
        let fields = parse_credential_output(input);
        assert_eq!(fields.get("username"), Some(&"user".to_string()));
        assert_eq!(fields.get("password"), Some(&"ghp_xxx".to_string()));
    }

    #[test]
    fn format_fields_for_stdin_output() {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        let output = format_fields_for_stdin(&fields);
        assert!(output.contains("protocol=https\n"));
        assert!(output.ends_with("\n\n"));
    }
}
