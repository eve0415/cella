//! Host-side validation of network rules.
//!
//! Runs at config load so a mistake in a `respond` rule or in
//! `response_headers` aborts `cella up` with a diagnostic instead of reaching
//! the agent. Checks that need the original `body_file` live in
//! `cella-config`, which still knows about the file.

use std::collections::{BTreeMap, BTreeSet};

use miette::Diagnostic;
use thiserror::Error;

use crate::config::{NetworkConfig, NetworkRule, RuleAction, status_forbids_body};

/// Headers the proxy manages itself (connection framing); configuring them
/// would conflict with what hyper writes.
const FORBIDDEN_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "content-length",
];

/// Most headers one `responses[].headers` or `response_headers` map may set.
pub const MAX_HEADERS: usize = 64;

/// Longest header name, in bytes, a rule may configure.
pub const MAX_HEADER_NAME_LEN: usize = 256;

/// Longest header value, in bytes, a rule may configure.
pub const MAX_HEADER_VALUE_LEN: usize = 8192;

/// A network rule that cannot be enforced as written.
#[derive(Debug, Error, Diagnostic)]
pub enum NetworkConfigError {
    #[error("network rule `{rule}`: action \"respond\" requires at least one entry in `responses`")]
    #[diagnostic(code(cella::network::missing_responses))]
    MissingResponses { rule: String },

    #[error("network rule `{rule}`: `responses` is only valid with action \"respond\"")]
    #[diagnostic(code(cella::network::responses_without_respond))]
    ResponsesWithoutRespond { rule: String },

    #[error("network rule `{rule}`: `response_headers` is only valid with action \"allow\"")]
    #[diagnostic(code(cella::network::response_headers_without_allow))]
    ResponseHeadersWithoutAllow { rule: String },

    #[error("network rule `{rule}`: `{field}` is {status}, which is outside 200..=599")]
    #[diagnostic(code(cella::network::invalid_status))]
    InvalidStatus {
        rule: String,
        field: String,
        status: u16,
    },

    #[error("network rule `{rule}`: `{field}` must be empty because status {status} has no body")]
    #[diagnostic(code(cella::network::body_not_allowed))]
    BodyNotAllowed {
        rule: String,
        field: String,
        status: u16,
    },

    #[error("network rule `{rule}`: header name {name:?} in `{field}` is not a valid HTTP token")]
    #[diagnostic(code(cella::network::invalid_header_name))]
    InvalidHeaderName {
        rule: String,
        field: String,
        name: String,
    },

    #[error(
        "network rule `{rule}`: header {name:?} in `{field}` has an invalid value \
         (only visible ASCII, space and tab are allowed, with no leading or trailing whitespace)"
    )]
    #[diagnostic(code(cella::network::invalid_header_value))]
    InvalidHeaderValue {
        rule: String,
        field: String,
        name: String,
    },

    #[error(
        "network rule `{rule}`: header {name:?} in `{field}` is set by the proxy and cannot be configured"
    )]
    #[diagnostic(code(cella::network::forbidden_header))]
    ForbiddenHeader {
        rule: String,
        field: String,
        name: String,
    },

    #[error(
        "network rule `{rule}`: `{field}` sets {count} headers, more than the limit of {max}",
        max = MAX_HEADERS
    )]
    #[diagnostic(code(cella::network::too_many_headers))]
    TooManyHeaders {
        rule: String,
        field: String,
        count: usize,
    },

    #[error(
        "network rule `{rule}`: header name {name:?} in `{field}` is {len} bytes, more than the limit of {max}",
        max = MAX_HEADER_NAME_LEN
    )]
    #[diagnostic(code(cella::network::header_name_too_long))]
    HeaderNameTooLong {
        rule: String,
        field: String,
        name: String,
        len: usize,
    },

    #[error(
        "network rule `{rule}`: header {name:?} in `{field}` has a {len}-byte value, more than the limit of {max}",
        max = MAX_HEADER_VALUE_LEN
    )]
    #[diagnostic(code(cella::network::header_value_too_long))]
    HeaderValueTooLong {
        rule: String,
        field: String,
        name: String,
        len: usize,
    },

    #[error(
        "network rule `{rule}`: header {name:?} in `{field}` is set more than once (header names are case-insensitive)"
    )]
    #[diagnostic(code(cella::network::duplicate_header))]
    DuplicateHeader {
        rule: String,
        field: String,
        name: String,
    },
}

impl NetworkConfig {
    /// Check every rule for settings the proxy cannot honour.
    ///
    /// # Errors
    ///
    /// Returns the first [`NetworkConfigError`] found, naming the rule by its
    /// domain and paths and the offending field.
    pub fn validate(&self) -> Result<(), NetworkConfigError> {
        self.rules.iter().try_for_each(validate_rule)
    }
}

fn validate_rule(rule: &NetworkRule) -> Result<(), NetworkConfigError> {
    match rule.action {
        RuleAction::Respond if rule.responses.is_empty() => {
            return Err(NetworkConfigError::MissingResponses { rule: rule.label() });
        }
        RuleAction::Block | RuleAction::Allow if !rule.responses.is_empty() => {
            return Err(NetworkConfigError::ResponsesWithoutRespond { rule: rule.label() });
        }
        _ => {}
    }
    if rule.action != RuleAction::Allow && !rule.response_headers.is_empty() {
        return Err(NetworkConfigError::ResponseHeadersWithoutAllow { rule: rule.label() });
    }
    validate_headers(rule, "response_headers", &rule.response_headers)?;

    for (index, response) in rule.responses.iter().enumerate() {
        if !(200..=599).contains(&response.status) {
            return Err(NetworkConfigError::InvalidStatus {
                rule: rule.label(),
                field: format!("responses[{index}].status"),
                status: response.status,
            });
        }
        if status_forbids_body(response.status) && !response.body.is_empty() {
            return Err(NetworkConfigError::BodyNotAllowed {
                rule: rule.label(),
                field: format!("responses[{index}].body"),
                status: response.status,
            });
        }
        validate_headers(
            rule,
            &format!("responses[{index}].headers"),
            &response.headers,
        )?;
    }
    Ok(())
}

fn validate_headers(
    rule: &NetworkRule,
    field: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(), NetworkConfigError> {
    if headers.len() > MAX_HEADERS {
        return Err(NetworkConfigError::TooManyHeaders {
            rule: rule.label(),
            field: field.to_string(),
            count: headers.len(),
        });
    }
    let err_fields = |name: &str| (rule.label(), field.to_string(), name.to_string());
    let mut seen = BTreeSet::new();
    for (name, value) in headers {
        if name.len() > MAX_HEADER_NAME_LEN {
            let (rule, field, name) = err_fields(name);
            let len = name.len();
            return Err(NetworkConfigError::HeaderNameTooLong {
                rule,
                field,
                name,
                len,
            });
        }
        if !is_token(name) {
            let (rule, field, name) = err_fields(name);
            return Err(NetworkConfigError::InvalidHeaderName { rule, field, name });
        }
        if FORBIDDEN_HEADERS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            let (rule, field, name) = err_fields(name);
            return Err(NetworkConfigError::ForbiddenHeader { rule, field, name });
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            let (rule, field, name) = err_fields(name);
            return Err(NetworkConfigError::DuplicateHeader { rule, field, name });
        }
        if value.len() > MAX_HEADER_VALUE_LEN {
            let (rule, field, name) = err_fields(name);
            return Err(NetworkConfigError::HeaderValueTooLong {
                rule,
                field,
                name,
                len: value.len(),
            });
        }
        if !is_valid_header_value(value) {
            let (rule, field, name) = err_fields(name);
            return Err(NetworkConfigError::InvalidHeaderValue { rule, field, name });
        }
    }
    Ok(())
}

/// RFC 9110 `token`: one or more `tchar`.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Visible ASCII plus interior space and tab: a strict subset of what the
/// `http` crate's `HeaderValue` accepts, so a value passing here always builds
/// in the agent.
fn is_valid_header_value(value: &str) -> bool {
    let is_ws = |b: u8| b == b' ' || b == b'\t';
    let bytes = value.as_bytes();
    let edges_ok =
        !bytes.first().copied().is_some_and(is_ws) && !bytes.last().copied().is_some_and(is_ws);
    edges_ok
        && bytes
            .iter()
            .all(|&b| b == b'\t' || (0x20..=0x7E).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResponseSpec;

    fn response(status: u16) -> ResponseSpec {
        ResponseSpec {
            status,
            headers: BTreeMap::new(),
            body: String::new(),
        }
    }

    fn respond_rule(responses: Vec<ResponseSpec>) -> NetworkRule {
        NetworkRule {
            domain: "api.example.com".to_string(),
            paths: vec!["/v1/messages".to_string()],
            action: RuleAction::Respond,
            responses,
            ..Default::default()
        }
    }

    fn validate(rule: NetworkRule) -> Result<(), NetworkConfigError> {
        NetworkConfig {
            rules: vec![rule],
            ..Default::default()
        }
        .validate()
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn with_header(name: &str, value: &str) -> NetworkRule {
        let mut r = response(200);
        r.headers = headers(&[(name, value)]);
        respond_rule(vec![r])
    }

    #[test]
    fn valid_respond_and_allow_rules_pass() {
        let mut first = response(529);
        first.headers = headers(&[("content-type", "application/json"), ("retry-after", "0")]);
        first.body = "{}".to_string();
        let allow = NetworkRule {
            domain: "api.example.com".to_string(),
            action: RuleAction::Allow,
            response_headers: headers(&[("x-test-run", "1")]),
            ..Default::default()
        };
        let config = NetworkConfig {
            rules: vec![respond_rule(vec![first, response(204)]), allow],
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn respond_without_responses_is_rejected() {
        let err = validate(respond_rule(vec![])).unwrap_err();
        assert!(matches!(err, NetworkConfigError::MissingResponses { .. }));
        assert!(
            err.to_string().contains("api.example.com [/v1/messages]"),
            "diagnostic must name the rule by domain and paths: {err}"
        );
    }

    #[test]
    fn responses_on_block_rule_are_rejected() {
        let mut rule = respond_rule(vec![response(200)]);
        rule.action = RuleAction::Block;
        let err = validate(rule).unwrap_err();
        assert!(matches!(
            err,
            NetworkConfigError::ResponsesWithoutRespond { .. }
        ));
    }

    #[test]
    fn response_headers_on_non_allow_rule_are_rejected() {
        let rule = NetworkRule {
            domain: "api.example.com".to_string(),
            action: RuleAction::Block,
            response_headers: headers(&[("x-test", "1")]),
            ..Default::default()
        };
        let err = validate(rule).unwrap_err();
        assert!(matches!(
            err,
            NetworkConfigError::ResponseHeadersWithoutAllow { .. }
        ));
    }

    #[test]
    fn status_outside_range_is_rejected() {
        for status in [199, 600] {
            let err = validate(respond_rule(vec![response(200), response(status)])).unwrap_err();
            assert!(
                matches!(err, NetworkConfigError::InvalidStatus { ref field, .. } if field == "responses[1].status"),
                "status {status}: {err}"
            );
        }
        for status in [200, 599] {
            validate(respond_rule(vec![response(status)])).unwrap();
        }
    }

    #[test]
    fn body_on_no_content_status_is_rejected() {
        for status in [204, 205, 304] {
            let mut r = response(status);
            r.body = "x".to_string();
            let err = validate(respond_rule(vec![r])).unwrap_err();
            assert!(
                matches!(err, NetworkConfigError::BodyNotAllowed { .. }),
                "status {status}: {err}"
            );
        }
    }

    #[test]
    fn invalid_header_name_is_rejected() {
        for name in ["", "bad header", "bad:name", "caf\u{e9}"] {
            let err = validate(with_header(name, "1")).unwrap_err();
            assert!(
                matches!(err, NetworkConfigError::InvalidHeaderName { .. }),
                "name {name:?}: {err}"
            );
        }
    }

    #[test]
    fn invalid_header_value_is_rejected() {
        for value in [" lead", "trail\t", "caf\u{e9}", "line\nbreak", "nul\0"] {
            let err = validate(with_header("x-test", value)).unwrap_err();
            assert!(
                matches!(err, NetworkConfigError::InvalidHeaderValue { .. }),
                "value {value:?}: {err}"
            );
        }
        validate(with_header("x-test", "a b\tc")).unwrap();
        validate(with_header("x-test", "")).unwrap();
    }

    #[test]
    fn framing_headers_are_rejected_case_insensitively() {
        for name in FORBIDDEN_HEADERS {
            let err = validate(with_header(&name.to_ascii_uppercase(), "1")).unwrap_err();
            assert!(
                matches!(err, NetworkConfigError::ForbiddenHeader { .. }),
                "header {name}: {err}"
            );
        }
        let allow = NetworkRule {
            domain: "api.example.com".to_string(),
            action: RuleAction::Allow,
            response_headers: headers(&[("Content-Length", "1")]),
            ..Default::default()
        };
        let err = validate(allow).unwrap_err();
        assert!(
            matches!(err, NetworkConfigError::ForbiddenHeader { ref field, .. } if field == "response_headers")
        );
    }

    #[test]
    fn case_variant_duplicate_header_is_rejected() {
        let mut r = response(200);
        r.headers = headers(&[("Content-Type", "a"), ("content-type", "b")]);
        let err = validate(respond_rule(vec![r])).unwrap_err();
        assert!(matches!(err, NetworkConfigError::DuplicateHeader { .. }));
    }

    fn numbered_headers(count: usize) -> BTreeMap<String, String> {
        (0..count)
            .map(|i| (format!("x-h{i}"), "1".to_string()))
            .collect()
    }

    fn allow_with_headers(count: usize) -> NetworkRule {
        NetworkRule {
            domain: "api.example.com".to_string(),
            action: RuleAction::Allow,
            response_headers: numbered_headers(count),
            ..Default::default()
        }
    }

    fn respond_with_headers(count: usize) -> NetworkRule {
        let mut r = response(200);
        r.headers = numbered_headers(count);
        respond_rule(vec![r])
    }

    #[test]
    fn header_count_is_limited_to_64_per_map() {
        validate(allow_with_headers(64)).unwrap();
        validate(respond_with_headers(64)).unwrap();

        let err = validate(allow_with_headers(65)).unwrap_err();
        assert!(
            matches!(err, NetworkConfigError::TooManyHeaders { ref field, count: 65, .. } if field == "response_headers"),
            "{err}"
        );
        assert!(err.to_string().contains("api.example.com"), "{err}");

        let err = validate(respond_with_headers(65)).unwrap_err();
        assert!(
            matches!(err, NetworkConfigError::TooManyHeaders { ref field, count: 65, .. } if field == "responses[0].headers"),
            "{err}"
        );
    }

    /// One rule per map, each setting the single header `name: value`, paired
    /// with the field name the diagnostic must report.
    fn single_header_rules(name: &str, value: &str) -> [(NetworkRule, &'static str); 2] {
        let allow = NetworkRule {
            domain: "api.example.com".to_string(),
            action: RuleAction::Allow,
            response_headers: headers(&[(name, value)]),
            ..Default::default()
        };
        [
            (allow, "response_headers"),
            (with_header(name, value), "responses[0].headers"),
        ]
    }

    #[test]
    fn header_name_is_limited_to_256_bytes() {
        for (rule, _) in single_header_rules(&"x".repeat(256), "1") {
            validate(rule).unwrap();
        }
        let long = "x".repeat(257);
        for (rule, expected_field) in single_header_rules(&long, "1") {
            let err = validate(rule).unwrap_err();
            assert!(
                matches!(
                    err,
                    NetworkConfigError::HeaderNameTooLong { ref field, ref name, len: 257, .. }
                        if field == expected_field && *name == long
                ),
                "{expected_field}: {err}"
            );
            assert!(err.to_string().contains("api.example.com"), "{err}");
        }
    }

    #[test]
    fn header_value_is_limited_to_8192_bytes() {
        for (rule, _) in single_header_rules("x-test", &"v".repeat(8192)) {
            validate(rule).unwrap();
        }
        for (rule, expected_field) in single_header_rules("x-test", &"v".repeat(8193)) {
            let err = validate(rule).unwrap_err();
            assert!(
                matches!(
                    err,
                    NetworkConfigError::HeaderValueTooLong { ref field, ref name, len: 8193, .. }
                        if field == expected_field && name == "x-test"
                ),
                "{expected_field}: {err}"
            );
            assert!(err.to_string().contains("api.example.com"), "{err}");
        }
    }
}
