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
