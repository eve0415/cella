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
