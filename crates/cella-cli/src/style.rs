//! Centralized terminal styling conventions using `owo-colors`.
//!
//! All user-facing color and formatting goes through these helpers so
//! the codebase has a single source of truth for what bold, dim, green,
//! etc. mean in cella's UI.  `owo-colors` respects `NO_COLOR` and
//! `FORCE_COLOR` environment variables automatically.

use owo_colors::OwoColorize;

// ── Label / value helpers ────────────────────────────────────────────

/// Bold text for field labels ("Template:", "Features:", etc.).
pub fn label(text: &str) -> String {
    format!("{}", text.bold())
}

/// Cyan text for values (template names, file paths, etc.).
pub fn value(text: &str) -> String {
    format!("{}", text.cyan())
}

/// Dim text for secondary information (descriptions, hints).
pub fn dim(text: &str) -> String {
    format!("{}", text.dimmed())
}

// ── Status symbols ───────────────────────────────────────────────────

/// Green checkmark for success.
pub fn success_mark() -> String {
    format!("{}", "\u{2713}".green())
}

/// Red cross for failure.
pub fn fail_mark() -> String {
    format!("{}", "\u{2717}".red())
}

/// Yellow warning triangle.
pub fn warn_mark() -> String {
    format!("{}", "\u{26a0}".yellow())
}

/// Dim arrow for hints.
pub fn hint_arrow() -> String {
    format!("{}", "\u{2192}".dimmed())
}
