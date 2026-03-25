//! Source text and on-demand span lookup for diagnostic rendering.

/// A byte range in the source text.
#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub offset: usize,
    pub length: usize,
}

/// Holds the original JSONC source and the cleaned JSON for span lookups.
#[derive(Debug)]
pub struct SourceText {
    original: String,
    cleaned: String,
    name: String,
}

impl SourceText {
    pub fn new(name: String, original: String, cleaned: String) -> Self {
        debug_assert_eq!(
            original.len(),
            cleaned.len(),
            "cleaned text must be same length as original"
        );
        Self {
            original,
            cleaned,
            name,
        }
    }

    pub fn original(&self) -> &str {
        &self.original
    }

    pub fn cleaned(&self) -> &str {
        &self.cleaned
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Find the byte span of a key at the given JSON pointer path.
    ///
    /// Path segments correspond to JSON pointer components,
    /// e.g., `["build", "dockerfile"]`.
    pub fn find_key_span(&self, path: &[&str]) -> Option<Range> {
        let bytes = self.cleaned.as_bytes();
        let pos = navigate_to_key(bytes, path)?;

        // pos points to the opening quote of the key
        let key_start = pos;
        let key_end = key_start + 1 + segment_byte_len(bytes, key_start + 1);
        Some(Range {
            offset: key_start,
            length: key_end - key_start + 1, // include closing quote
        })
    }

    /// Find the byte span of the value at the given JSON pointer path.
    pub fn find_value_span(&self, path: &[&str]) -> Option<Range> {
        let bytes = self.cleaned.as_bytes();
        let pos = navigate_to_key(bytes, path)?;

        // Skip past the last key and colon to find the value
        let pos = skip_past_colon(bytes, pos)?;
        let value_start = skip_whitespace(bytes, pos);
        let value_end = find_value_end(bytes, value_start)?;

        Some(Range {
            offset: value_start,
            length: value_end - value_start,
        })
    }

    /// Create a `miette::NamedSource` for diagnostic rendering.
    pub fn as_named_source(&self) -> miette::NamedSource<String> {
        miette::NamedSource::new(&self.name, self.original.clone())
    }
}

/// Walk a JSON pointer path to find the position of the final key.
fn navigate_to_key(bytes: &[u8], path: &[&str]) -> Option<usize> {
    let mut pos = 0;
    for (i, &segment) in path.iter().enumerate() {
        pos = find_key_in_object(bytes, pos, segment)?;
        if i < path.len() - 1 {
            pos = skip_past_colon(bytes, pos)?;
            pos = skip_whitespace(bytes, pos);
        }
    }
    Some(pos)
}

/// Forward-scan to find a key in an object starting from `pos`.
fn find_key_in_object(bytes: &[u8], mut pos: usize, key: &str) -> Option<usize> {
    // Find the opening `{`
    pos = skip_whitespace(bytes, pos);
    if pos >= bytes.len() || bytes[pos] != b'{' {
        return None;
    }
    pos += 1; // skip `{`

    loop {
        pos = skip_whitespace(bytes, pos);
        if pos >= bytes.len() || bytes[pos] == b'}' {
            return None;
        }

        if bytes[pos] == b'"' {
            let key_start = pos;
            let parsed_key = read_json_string(bytes, pos)?;
            pos = skip_string(bytes, pos)?;

            if parsed_key == key {
                return Some(key_start);
            }

            // Skip past the colon and value
            pos = skip_past_colon(bytes, pos)?;
            pos = skip_value(bytes, pos)?;

            // Skip comma if present
            let next = skip_whitespace(bytes, pos);
            if next < bytes.len() && bytes[next] == b',' {
                pos = next + 1;
            } else {
                pos = next;
            }
        } else {
            return None; // unexpected character
        }
    }
}

fn skip_whitespace(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

fn skip_past_colon(bytes: &[u8], mut pos: usize) -> Option<usize> {
    // Skip past the key string
    pos = skip_string(bytes, pos)?;
    pos = skip_whitespace(bytes, pos);
    if pos < bytes.len() && bytes[pos] == b':' {
        Some(pos + 1)
    } else {
        None
    }
}

fn skip_string(bytes: &[u8], mut pos: usize) -> Option<usize> {
    if pos >= bytes.len() || bytes[pos] != b'"' {
        return None;
    }
    pos += 1; // skip opening quote
    while pos < bytes.len() {
        if bytes[pos] == b'\\' {
            pos += 2; // skip escape sequence
        } else if bytes[pos] == b'"' {
            return Some(pos + 1);
        } else {
            pos += 1;
        }
    }
    None // unterminated string
}

fn read_json_string(bytes: &[u8], pos: usize) -> Option<String> {
    if pos >= bytes.len() || bytes[pos] != b'"' {
        return None;
    }
    let end = skip_string(bytes, pos)?;
    // Extract the content between quotes
    let content = &bytes[pos + 1..end - 1];
    std::str::from_utf8(content).ok().map(String::from)
}

fn segment_byte_len(bytes: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < bytes.len() && bytes[i] != b'"' {
        if bytes[i] == b'\\' {
            i += 2;
        } else {
            i += 1;
        }
    }
    i - pos
}

fn skip_value(bytes: &[u8], mut pos: usize) -> Option<usize> {
    pos = skip_whitespace(bytes, pos);
    if pos >= bytes.len() {
        return None;
    }

    match bytes[pos] {
        b'"' => skip_string(bytes, pos),
        b'{' => skip_balanced(bytes, pos, b'{', b'}'),
        b'[' => skip_balanced(bytes, pos, b'[', b']'),
        b't' | b'f' | b'n' => {
            // true, false, null
            while pos < bytes.len() && bytes[pos].is_ascii_alphanumeric() {
                pos += 1;
            }
            Some(pos)
        }
        _ => {
            // number
            while pos < bytes.len()
                && (bytes[pos].is_ascii_digit()
                    || bytes[pos] == b'.'
                    || bytes[pos] == b'-'
                    || bytes[pos] == b'+'
                    || bytes[pos] == b'e'
                    || bytes[pos] == b'E')
            {
                pos += 1;
            }
            Some(pos)
        }
    }
}

fn skip_balanced(bytes: &[u8], mut pos: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;

    while pos < bytes.len() {
        if in_string {
            if bytes[pos] == b'\\' {
                pos += 2;
                continue;
            }
            if bytes[pos] == b'"' {
                in_string = false;
            }
        } else if bytes[pos] == b'"' {
            in_string = true;
        } else if bytes[pos] == open {
            depth += 1;
        } else if bytes[pos] == close {
            depth -= 1;
            if depth == 0 {
                return Some(pos + 1);
            }
        }
        pos += 1;
    }
    None
}

fn find_value_end(bytes: &[u8], pos: usize) -> Option<usize> {
    skip_value(bytes, pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(json: &str) -> SourceText {
        SourceText::new("test.json".into(), json.into(), json.into())
    }

    #[test]
    fn test_find_key_span_simple() {
        let s = source(r#"{"name": "cella"}"#);
        let span = s.find_key_span(&["name"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"name\""
        );
    }

    #[test]
    fn test_find_value_span_simple() {
        let s = source(r#"{"name": "cella"}"#);
        let span = s.find_value_span(&["name"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"cella\""
        );
    }

    #[test]
    fn test_find_nested() {
        let s = source(r#"{"build": {"dockerfile": "Dockerfile"}}"#);
        let span = s.find_value_span(&["build", "dockerfile"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"Dockerfile\""
        );
    }

    #[test]
    fn test_missing_key() {
        let s = source(r#"{"name": "cella"}"#);
        assert!(s.find_key_span(&["missing"]).is_none());
    }

    #[test]
    fn test_deeply_nested_path() {
        let s = source(r#"{"a": {"b": {"c": "deep"}}}"#);
        let span = s.find_value_span(&["a", "b", "c"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"deep\""
        );
    }

    #[test]
    fn test_array_value_span() {
        let s = source(r#"{"arr": [1, 2, 3]}"#);
        let span = s.find_value_span(&["arr"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "[1, 2, 3]"
        );
    }

    #[test]
    fn test_object_value_span() {
        let s = source(r#"{"obj": {"a": 1}}"#);
        let span = s.find_value_span(&["obj"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "{\"a\": 1}"
        );
    }

    #[test]
    fn test_boolean_value_span() {
        let s = source(r#"{"flag": true}"#);
        let span = s.find_value_span(&["flag"]).unwrap();
        assert_eq!(&s.cleaned()[span.offset..span.offset + span.length], "true");
    }

    #[test]
    fn test_null_value_span() {
        let s = source(r#"{"val": null}"#);
        let span = s.find_value_span(&["val"]).unwrap();
        assert_eq!(&s.cleaned()[span.offset..span.offset + span.length], "null");
    }

    #[test]
    fn test_number_value_span() {
        let s = source(r#"{"num": 42}"#);
        let span = s.find_value_span(&["num"]).unwrap();
        assert_eq!(&s.cleaned()[span.offset..span.offset + span.length], "42");
    }

    #[test]
    fn test_negative_number_value_span() {
        let s = source(r#"{"num": -1}"#);
        let span = s.find_value_span(&["num"]).unwrap();
        assert_eq!(&s.cleaned()[span.offset..span.offset + span.length], "-1");
    }

    #[test]
    fn test_escaped_chars_in_key() {
        let s = source(r#"{"a\\b": "val"}"#);
        let span = s.find_key_span(&["a\\\\b"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"a\\\\b\""
        );
    }

    #[test]
    fn test_multiple_keys_first_found() {
        // The scanner can find the first key in an object
        let s = source(r#"{"first": "a", "second": "b"}"#);
        let span = s.find_value_span(&["first"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"a\""
        );
    }

    #[test]
    fn test_whitespace_around_values() {
        let s = source(r#"{"key":   "value"   }"#);
        let span = s.find_value_span(&["key"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"value\""
        );
    }

    #[test]
    fn test_empty_object_returns_none() {
        let s = source(r"{}");
        assert!(s.find_key_span(&["anything"]).is_none());
    }

    #[test]
    fn test_find_key_span_nested() {
        let s = source(r#"{"outer": {"inner": "val"}}"#);
        let span = s.find_key_span(&["outer", "inner"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"inner\""
        );
    }

    #[test]
    fn test_value_span_string_with_quotes() {
        let s = source(r#"{"msg": "hello world"}"#);
        let span = s.find_value_span(&["msg"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"hello world\""
        );
    }

    #[test]
    fn test_find_value_span_nested_object() {
        let s = source(r#"{"outer": {"a": "x", "b": "y"}}"#);
        let span = s.find_value_span(&["outer"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            r#"{"a": "x", "b": "y"}"#
        );
    }

    #[test]
    fn test_key_span_only_key_in_object() {
        // Verify key span works for a single-key object
        let s = source(r#"{"beta": "y"}"#);
        let span = s.find_key_span(&["beta"]).unwrap();
        assert_eq!(
            &s.cleaned()[span.offset..span.offset + span.length],
            "\"beta\""
        );
    }
}
