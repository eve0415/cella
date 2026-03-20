//! Maps validation errors to developer-friendly diagnostics with source spans.
#![allow(unused_assignments)]

use std::fmt::Write;

use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme, Report};
use thiserror::Error;

use crate::span::{ByteSpan, SourceText};

/// Severity level for a config diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A single diagnostic from config validation.
#[derive(Debug, Clone)]
pub struct ConfigDiagnostic {
    pub severity: Severity,
    pub message: String,
    pub path: String,
    pub span: Option<ByteSpan>,
    pub help: Option<String>,
}

/// A renderable diagnostic with source context, implementing `miette::Diagnostic`.
#[derive(Debug, Error, Diagnostic)]
#[error("{message}")]
#[allow(unused_assignments, clippy::missing_const_for_fn)]
pub struct RenderableDiagnostic {
    message: String,

    #[source_code]
    src: miette::NamedSource<String>,

    #[label("{label}")]
    span: Option<miette::SourceSpan>,

    label: String,

    #[help]
    help: Option<String>,
}

/// Collection of diagnostics with source context for rendering.
#[derive(Debug)]
pub struct ConfigDiagnostics {
    source: SourceText,
    diagnostics: Vec<ConfigDiagnostic>,
}

impl ConfigDiagnostics {
    pub const fn new(source: SourceText, diagnostics: Vec<ConfigDiagnostic>) -> Self {
        Self {
            source,
            diagnostics,
        }
    }

    pub fn diagnostics(&self) -> &[ConfigDiagnostic] {
        &self.diagnostics
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }

    /// Render all diagnostics to a string using miette's graphical renderer.
    pub fn render(&self) -> String {
        let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode());
        let mut output = String::new();

        for diag in &self.diagnostics {
            let renderable = RenderableDiagnostic {
                message: diag.message.clone(),
                src: self.source.as_named_source(),
                span: diag
                    .span
                    .map(|s| miette::SourceSpan::new(s.offset.into(), s.length)),
                label: diag.path.clone(),
                help: diag.help.clone(),
            };

            let report = Report::new(renderable);
            let mut buf = String::new();
            if handler.render_report(&mut buf, report.as_ref()).is_ok() {
                output.push_str(&buf);
                output.push('\n');
            }
        }

        let errors = self.error_count();
        let warnings = self.warning_count();
        let _ = writeln!(
            output,
            "{errors} error(s), {warnings} warning(s) in {}",
            self.source.name()
        );

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SourceText;

    fn dummy_source() -> SourceText {
        let text = r#"{"image": "ubuntu"}"#.to_string();
        SourceText::new("test.json".into(), text.clone(), text)
    }

    fn make_error(msg: &str, path: &str) -> ConfigDiagnostic {
        ConfigDiagnostic {
            severity: Severity::Error,
            message: msg.to_string(),
            path: path.to_string(),
            span: None,
            help: None,
        }
    }

    fn make_warning(msg: &str, path: &str) -> ConfigDiagnostic {
        ConfigDiagnostic {
            severity: Severity::Warning,
            message: msg.to_string(),
            path: path.to_string(),
            span: None,
            help: None,
        }
    }

    #[test]
    fn has_errors_true_with_error() {
        let diags = ConfigDiagnostics::new(dummy_source(), vec![make_error("bad", "$.image")]);
        assert!(diags.has_errors());
    }

    #[test]
    fn has_errors_false_warnings_only() {
        let diags = ConfigDiagnostics::new(dummy_source(), vec![make_warning("meh", "$.image")]);
        assert!(!diags.has_errors());
    }

    #[test]
    fn has_errors_false_empty() {
        let diags = ConfigDiagnostics::new(dummy_source(), vec![]);
        assert!(!diags.has_errors());
    }

    #[test]
    fn error_count_mixed() {
        let diags = ConfigDiagnostics::new(
            dummy_source(),
            vec![
                make_error("e1", "$.a"),
                make_error("e2", "$.b"),
                make_warning("w1", "$.c"),
            ],
        );
        assert_eq!(diags.error_count(), 2);
    }

    #[test]
    fn error_count_zero() {
        let diags = ConfigDiagnostics::new(
            dummy_source(),
            vec![make_warning("w1", "$.a"), make_warning("w2", "$.b")],
        );
        assert_eq!(diags.error_count(), 0);
    }

    #[test]
    fn warning_count_mixed() {
        let diags = ConfigDiagnostics::new(
            dummy_source(),
            vec![
                make_error("e1", "$.a"),
                make_warning("w1", "$.b"),
                make_warning("w2", "$.c"),
            ],
        );
        assert_eq!(diags.warning_count(), 2);
    }

    #[test]
    fn warning_count_zero() {
        let diags = ConfigDiagnostics::new(
            dummy_source(),
            vec![make_error("e1", "$.a"), make_error("e2", "$.b")],
        );
        assert_eq!(diags.warning_count(), 0);
    }

    #[test]
    fn render_contains_message() {
        let diags =
            ConfigDiagnostics::new(dummy_source(), vec![make_error("invalid image", "$.image")]);
        let output = diags.render();
        assert!(
            output.contains("invalid image"),
            "render output should contain the diagnostic message"
        );
    }

    #[test]
    fn render_contains_source_name() {
        let diags =
            ConfigDiagnostics::new(dummy_source(), vec![make_error("bad value", "$.image")]);
        let output = diags.render();
        assert!(
            output.contains("test.json"),
            "render output should contain the source file name"
        );
    }

    #[test]
    fn render_contains_help() {
        let mut diag = make_error("bad value", "$.image");
        diag.help = Some("try using a valid image name".to_string());
        let diags = ConfigDiagnostics::new(dummy_source(), vec![diag]);
        let output = diags.render();
        assert!(
            output.contains("try using a valid image name"),
            "render output should contain the help text"
        );
    }

    #[test]
    fn render_with_span() {
        let mut diag = make_error("bad value", "$.image");
        diag.span = Some(ByteSpan {
            offset: 10,
            length: 8,
        });
        let diags = ConfigDiagnostics::new(dummy_source(), vec![diag]);
        let output = diags.render();
        // The label (path) should appear in the rendered output when a span is present
        assert!(
            output.contains("$.image"),
            "render output should contain the label from the span"
        );
    }

    #[test]
    fn render_without_span() {
        let diags =
            ConfigDiagnostics::new(dummy_source(), vec![make_error("no span here", "$.image")]);
        let output = diags.render();
        assert!(
            output.contains("no span here"),
            "render should work even without a span"
        );
    }

    #[test]
    fn render_empty_diagnostics() {
        let diags = ConfigDiagnostics::new(dummy_source(), vec![]);
        let output = diags.render();
        assert!(
            output.contains("0 error(s), 0 warning(s)"),
            "empty diagnostics should show zero counts"
        );
    }

    #[test]
    fn render_summary_line() {
        let diags = ConfigDiagnostics::new(
            dummy_source(),
            vec![
                make_error("e1", "$.a"),
                make_error("e2", "$.b"),
                make_warning("w1", "$.c"),
            ],
        );
        let output = diags.render();
        assert!(
            output.contains("2 error(s), 1 warning(s)"),
            "render should contain the correct summary counts"
        );
    }
}
