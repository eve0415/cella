use clap::Args;

use cella_doctor::checks::{self, CategoryReport, CheckResult, Report, Severity};
use cella_doctor::redact::Redactor;

/// Check system dependencies and configuration.
#[derive(Args)]
pub struct DoctorArgs {
    /// Check all running cella containers (not just current workspace).
    #[arg(long)]
    all: bool,
    /// Output as JSON (machine-readable).
    #[arg(long)]
    json: bool,
    /// Disable redaction of sensitive information (home paths, tokens).
    #[arg(long)]
    no_redact: bool,
}

impl DoctorArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let workspace_folder = std::env::current_dir().ok();
        let ctx = checks::CheckContext::new(workspace_folder, self.all);
        let mut report = checks::run_all_checks(&ctx).await;

        if !self.no_redact {
            let redactor = Redactor::new();
            report.redact(&redactor);
        }

        if self.json {
            let json = report.to_json();
            println!(
                "{}",
                serde_json::to_string_pretty(&json).unwrap_or_default()
            );
        } else {
            print_report(&report);
        }

        if report.has_errors() {
            std::process::exit(1);
        }
        Ok(())
    }
}

fn print_report(report: &Report) {
    let mut first = true;
    for category in &report.categories {
        if !first {
            eprintln!();
        }
        first = false;
        print_category(category);
    }
}

fn print_category(category: &CategoryReport) {
    eprintln!("\x1b[1m{}\x1b[0m", category.name);
    for check in &category.checks {
        print_check(check);
    }
}

fn print_check(check: &CheckResult) {
    let (symbol, color) = match check.severity {
        Severity::Pass => ("\u{2713}", "\x1b[32m"),    // green ✓
        Severity::Warning => ("\u{26a0}", "\x1b[33m"), // yellow ⚠
        Severity::Error => ("\u{2717}", "\x1b[31m"),   // red ✗
        Severity::Info => (" ", ""),                   // no symbol
    };

    let reset = if color.is_empty() { "" } else { "\x1b[0m" };

    eprintln!(
        "  {color}{symbol}{reset} {:<24}{}",
        check.name, check.detail
    );

    if let Some(ref hint) = check.fix_hint {
        eprintln!("    \x1b[2m\u{2192} {hint}\x1b[0m");
    }
}
