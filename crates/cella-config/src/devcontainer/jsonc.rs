//! JSONC (JSON with Comments) preprocessor.
//!
//! Single-pass state machine that replaces comments and trailing commas with
//! whitespace. Key invariant: `output.len() == input.len()` -- byte offsets
//! are preserved exactly.

/// Errors that can occur during JSONC stripping.
#[derive(Debug)]
pub struct JsoncError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for JsoncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSONC error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsoncError {}

#[derive(PartialEq)]
enum State {
    Normal,
    InString,
    LineComment,
    BlockComment,
}

/// Strip JSONC comments and trailing commas, preserving byte offsets.
///
/// - `//` → replaced with spaces until newline (newline kept)
/// - `/* */` → replaced with spaces (newlines inside kept)
/// - Trailing `,` before `]` or `}` → replaced with space
/// - String contents: passed through, handles `\"` escapes
///
/// # Errors
///
/// Returns `JsoncError` if the input contains an unterminated block comment
/// or if comment stripping produces invalid UTF-8.
pub fn strip_jsonc(input: &str) -> Result<String, JsoncError> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = Vec::with_capacity(len);
    let mut i = 0;

    let mut state = State::Normal;

    while i < len {
        match state {
            State::Normal => {
                if bytes[i] == b'"' {
                    out.push(bytes[i]);
                    state = State::InString;
                    i += 1;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                    out.push(b' ');
                    out.push(b' ');
                    state = State::LineComment;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    out.push(b' ');
                    out.push(b' ');
                    state = State::BlockComment;
                    i += 2;
                } else if bytes[i] == b',' {
                    // Check if this is a trailing comma
                    if is_trailing_comma(bytes, i) {
                        out.push(b' ');
                    } else {
                        out.push(bytes[i]);
                    }
                    i += 1;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            State::InString => {
                out.push(bytes[i]);
                if bytes[i] == b'\\' && i + 1 < len {
                    // Escaped character — push next byte and skip
                    i += 1;
                    out.push(bytes[i]);
                } else if bytes[i] == b'"' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::LineComment => {
                if bytes[i] == b'\n' {
                    out.push(b'\n');
                    state = State::Normal;
                } else {
                    out.push(b' ');
                }
                i += 1;
            }
            State::BlockComment => {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    out.push(b' ');
                    out.push(b' ');
                    state = State::Normal;
                    i += 2;
                } else if bytes[i] == b'\n' {
                    out.push(b'\n');
                    i += 1;
                } else {
                    out.push(b' ');
                    i += 1;
                }
            }
        }
    }

    if state == State::BlockComment {
        return Err(JsoncError {
            message: "unterminated block comment".into(),
            offset: len,
        });
    }

    debug_assert_eq!(out.len(), len, "output length must equal input length");

    String::from_utf8(out).map_err(|e| JsoncError {
        message: format!("produced invalid UTF-8: {e}"),
        offset: 0,
    })
}

/// Check if the comma at position `pos` is a trailing comma
/// (only whitespace between it and the next `]` or `}`).
fn is_trailing_comma(bytes: &[u8], pos: usize) -> bool {
    let mut j = pos + 1;
    while j < bytes.len() {
        match bytes[j] {
            b' ' | b'\t' | b'\r' | b'\n' => j += 1,
            b']' | b'}' => return true,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_line_comment() {
        let input = r#"{"key": "value" // comment
}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        assert!(output.contains(r#""key": "value""#));
        assert!(!output.contains("//"));
    }

    #[test]
    fn test_strip_block_comment() {
        let input = r#"{"key": /* comment */ "value"}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_strip_trailing_comma() {
        let input = r#"{"a": 1, "b": 2, }"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn test_string_with_slash() {
        let input = r#"{"url": "http://example.com"}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn test_escaped_quote_in_string() {
        let input = r#"{"key": "val\"ue"}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn test_unterminated_block_comment() {
        let input = r#"{"key": /* unterminated"#;
        assert!(strip_jsonc(input).is_err());
    }

    #[test]
    fn test_preserves_newlines_in_block_comment() {
        let input = "{\n/* line1\nline2 */\n\"a\": 1\n}";
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        // Newlines inside the block comment should be preserved
        assert_eq!(
            output.chars().filter(|c| *c == '\n').count(),
            input.chars().filter(|c| *c == '\n').count()
        );
    }

    #[test]
    fn test_trailing_comma_in_array() {
        let input = r"[1, 2, 3, ]";
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_nested_block_comments() {
        // JSONC does not support nested block comments. The first `*/` ends the comment.
        // So `/* /* */ */` leaves ` */` as literal text.
        let input = r#"{"a": 1 /* /* */ , "b": 2}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        // After first `*/`, the rest is normal text, so `"b"` should be present
        assert!(output.contains("\"b\": 2"));
    }

    #[test]
    fn test_multiple_comments_same_line() {
        let input = r#"{"a": 1 /* c1 */ , "b": 2 /* c2 */}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn test_comment_like_in_string() {
        let input = r#"{"a": "// not a comment"}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn test_block_comment_syntax_in_string() {
        let input = r#"{"a": "/* not a comment */"}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn test_empty_input() {
        let output = strip_jsonc("").unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn test_whitespace_only() {
        let input = "   ";
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output, "   ");
    }

    #[test]
    fn test_line_comment_at_eof() {
        let input = r#"{"a": 1} // comment"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn test_multiple_trailing_commas_nested() {
        let input = r#"{"a": {"b": 1, }, }"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["a"]["b"], 1);
    }

    #[test]
    fn test_consecutive_line_comments() {
        let input = "// first\n// second\n{\"a\": 1}";
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        assert!(!output.contains("//"));
        let trimmed = output.trim();
        let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn test_multi_line_block_comment() {
        let input = "/*\n  multi\n  line\n*/\n{\"a\": 1}";
        let output = strip_jsonc(input).unwrap();
        assert_eq!(output.len(), input.len());
        // Newlines should be preserved
        assert_eq!(
            output.chars().filter(|c| *c == '\n').count(),
            input.chars().filter(|c| *c == '\n').count()
        );
    }

    #[test]
    fn test_single_slash_not_comment() {
        let input = r#"{"a": "x/y"}"#;
        let output = strip_jsonc(input).unwrap();
        assert_eq!(input, output);
    }
}
