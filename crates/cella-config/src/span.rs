//! Source text and on-demand span lookup for diagnostic rendering.

/// A byte range in the source text.
#[derive(Debug, Clone, Copy)]
pub struct ByteSpan {
    pub offset: usize,
    pub length: usize,
}

/// Holds the original JSONC source and the cleaned JSON for span lookups.
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
    pub fn find_key_span(&self, path: &[&str]) -> Option<ByteSpan> {
        let bytes = self.cleaned.as_bytes();
        let mut pos = 0;

        for (i, &segment) in path.iter().enumerate() {
            pos = find_key_in_object(bytes, pos, segment)?;
            if i < path.len() - 1 {
                // Advance past this key's value into the child object
                pos = skip_past_colon(bytes, pos)?;
                pos = skip_whitespace(bytes, pos);
            }
        }

        // pos points to the opening quote of the key
        let key_start = pos;
        let key_end = key_start + 1 + segment_byte_len(bytes, key_start + 1);
        Some(ByteSpan {
            offset: key_start,
            length: key_end - key_start + 1, // include closing quote
        })
    }

    /// Find the byte span of the value at the given JSON pointer path.
    pub fn find_value_span(&self, path: &[&str]) -> Option<ByteSpan> {
        let bytes = self.cleaned.as_bytes();
        let mut pos = 0;

        for (i, &segment) in path.iter().enumerate() {
            pos = find_key_in_object(bytes, pos, segment)?;
            if i < path.len() - 1 {
                // Advance past this key's value into the child object
                pos = skip_past_colon(bytes, pos)?;
                pos = skip_whitespace(bytes, pos);
            }
        }

        // Skip past the last key and colon to find the value
        pos = skip_past_colon(bytes, pos)?;
        let value_start = skip_whitespace(bytes, pos);
        let value_end = find_value_end(bytes, value_start)?;

        Some(ByteSpan {
            offset: value_start,
            length: value_end - value_start,
        })
    }

    /// Create a `miette::NamedSource` for diagnostic rendering.
    pub fn as_named_source(&self) -> miette::NamedSource<String> {
        miette::NamedSource::new(&self.name, self.original.clone())
    }
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
}
