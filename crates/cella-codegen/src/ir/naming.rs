use heck::{ToPascalCase, ToSnakeCase};

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield", "gen",
];

pub fn to_rust_field_name(json_name: &str) -> String {
    let snake = json_name.to_snake_case();
    if snake.is_empty() {
        return "unknown".to_string();
    }
    if RUST_KEYWORDS.contains(&snake.as_str()) {
        format!("r#{snake}")
    } else if snake.starts_with(char::is_numeric) {
        format!("f_{snake}")
    } else {
        snake
    }
}

pub fn to_rust_type_name(schema_name: &str) -> String {
    let pascal = schema_name.to_pascal_case();
    if pascal.is_empty() {
        "Unknown".to_string()
    } else if pascal.starts_with(char::is_numeric) {
        format!("T{pascal}")
    } else {
        pascal
    }
}

pub fn to_variant_name(value: &str) -> String {
    let pascal = value.to_pascal_case();
    if pascal.is_empty() {
        "Unknown".to_string()
    } else if pascal.starts_with(char::is_numeric) {
        format!("V{pascal}")
    } else {
        pascal
    }
}

/// Extract definition name from a `$ref` path like `#/definitions/fooBar`.
pub fn ref_to_def_name(ref_path: &str) -> Option<&str> {
    ref_path.strip_prefix("#/definitions/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── to_rust_field_name ──────────────────────────────────────────

    #[test]
    fn field_name_camel_case() {
        assert_eq!(to_rust_field_name("camelCase"), "camel_case");
    }

    #[test]
    fn field_name_keyword_type() {
        assert_eq!(to_rust_field_name("type"), "r#type");
    }

    #[test]
    fn field_name_numeric_prefix() {
        assert_eq!(to_rust_field_name("3d"), "f_3d");
    }

    #[test]
    fn field_name_empty() {
        assert_eq!(to_rust_field_name(""), "unknown");
    }

    #[test]
    fn field_name_hyphenated() {
        assert_eq!(to_rust_field_name("x-custom"), "x_custom");
    }

    #[test]
    fn field_name_pascal_case() {
        assert_eq!(to_rust_field_name("PascalCase"), "pascal_case");
    }

    #[test]
    fn field_name_already_snake() {
        assert_eq!(to_rust_field_name("already_snake"), "already_snake");
    }

    #[test]
    fn field_name_keyword_self() {
        assert_eq!(to_rust_field_name("self"), "r#self");
    }

    // ── to_rust_type_name ───────────────────────────────────────────

    #[test]
    fn type_name_camel_case() {
        assert_eq!(to_rust_type_name("fooBar"), "FooBar");
    }

    #[test]
    fn type_name_numeric_prefix() {
        assert_eq!(to_rust_type_name("3d"), "T3d");
    }

    #[test]
    fn type_name_empty() {
        assert_eq!(to_rust_type_name(""), "Unknown");
    }

    #[test]
    fn type_name_hyphenated() {
        assert_eq!(to_rust_type_name("already-pascal"), "AlreadyPascal");
    }

    #[test]
    fn type_name_snake_case() {
        assert_eq!(to_rust_type_name("snake_case"), "SnakeCase");
    }

    // ── to_variant_name ─────────────────────────────────────────────

    #[test]
    fn variant_name_hyphenated() {
        assert_eq!(to_variant_name("some-value"), "SomeValue");
    }

    #[test]
    fn variant_name_numeric_prefix() {
        assert_eq!(to_variant_name("3d"), "V3d");
    }

    #[test]
    fn variant_name_empty() {
        assert_eq!(to_variant_name(""), "Unknown");
    }

    #[test]
    fn variant_name_upper() {
        assert_eq!(to_variant_name("UPPER"), "Upper");
    }

    #[test]
    fn variant_name_simple() {
        assert_eq!(to_variant_name("simple"), "Simple");
    }

    // ── ref_to_def_name ─────────────────────────────────────────────

    #[test]
    fn ref_valid_foo() {
        assert_eq!(ref_to_def_name("#/definitions/Foo"), Some("Foo"));
    }

    #[test]
    fn ref_valid_camel() {
        assert_eq!(ref_to_def_name("#/definitions/fooBar"), Some("fooBar"));
    }

    #[test]
    fn ref_no_prefix() {
        assert_eq!(ref_to_def_name("Foo"), None);
    }

    #[test]
    fn ref_wrong_prefix() {
        assert_eq!(ref_to_def_name("#/defs/Foo"), None);
    }

    #[test]
    fn ref_empty() {
        assert_eq!(ref_to_def_name(""), None);
    }
}
