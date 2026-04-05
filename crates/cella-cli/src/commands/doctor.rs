use clap::Args;

use cella_doctor::checks::{self, CategoryReport, CheckResult, Report, Severity};
use cella_doctor::redact::Redactor;

use super::OutputFormat;

/// Check system dependencies and configuration.
#[derive(Args)]
pub struct DoctorArgs {
    /// Check all running cella containers (not just current workspace).
    #[arg(long)]
    all: bool,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
    /// Disable redaction of sensitive information (home paths, tokens).
    #[arg(long)]
    no_redact: bool,
    #[command(flatten)]
    backend: crate::backend::BackendArgs,
}

impl DoctorArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let workspace_folder = std::env::current_dir().ok();
        let (backend_client, backend_error) = match self.backend.resolve_client().await {
            Ok(client) => (Some(client), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let backend_kind = backend_client.as_ref().map(|c| c.kind()).or_else(|| {
            self.backend
                .backend
                .as_ref()
                .map(crate::backend::BackendChoice::to_kind)
        });
        let ctx =
            checks::CheckContext::new(workspace_folder, self.all, backend_kind, backend_client);
        let mut report = checks::run_all_checks(&ctx).await;

        // Surface backend connection failure when explicitly requested
        let explicit_backend = self.backend.backend.is_some() || self.backend.docker_host.is_some();
        if let Some(err) = backend_error.filter(|_| explicit_backend) {
            report.categories.insert(
                0,
                CategoryReport::new(
                    "Backend",
                    vec![CheckResult {
                        name: "connection".into(),
                        severity: Severity::Error,
                        detail: err,
                        fix_hint: None,
                    }],
                ),
            );
        }

        if !self.no_redact {
            let redactor = Redactor::new();
            report.redact(&redactor);
        }

        if matches!(self.output, OutputFormat::Json) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_check_pass_does_not_panic() {
        let check = CheckResult {
            name: "Docker".to_string(),
            detail: "installed".to_string(),
            severity: Severity::Pass,
            fix_hint: None,
        };
        print_check(&check);
    }

    #[test]
    fn print_check_warning_does_not_panic() {
        let check = CheckResult {
            name: "Docker".to_string(),
            detail: "old version".to_string(),
            severity: Severity::Warning,
            fix_hint: Some("upgrade Docker".to_string()),
        };
        print_check(&check);
    }

    #[test]
    fn print_check_error_does_not_panic() {
        let check = CheckResult {
            name: "Docker".to_string(),
            detail: "not found".to_string(),
            severity: Severity::Error,
            fix_hint: Some("install Docker".to_string()),
        };
        print_check(&check);
    }

    #[test]
    fn print_check_info_does_not_panic() {
        let check = CheckResult {
            name: "Platform".to_string(),
            detail: "linux/amd64".to_string(),
            severity: Severity::Info,
            fix_hint: None,
        };
        print_check(&check);
    }

    #[test]
    fn print_category_does_not_panic() {
        let category = CategoryReport {
            name: "System".to_string(),
            status: Severity::Pass,
            checks: vec![CheckResult {
                name: "OS".to_string(),
                detail: "Linux".to_string(),
                severity: Severity::Pass,
                fix_hint: None,
            }],
        };
        print_category(&category);
    }

    #[test]
    fn print_report_empty_does_not_panic() {
        let report = Report { categories: vec![] };
        print_report(&report);
    }

    #[test]
    fn print_report_multiple_categories() {
        let report = Report {
            categories: vec![
                CategoryReport {
                    name: "System".to_string(),
                    status: Severity::Pass,
                    checks: vec![],
                },
                CategoryReport {
                    name: "Docker".to_string(),
                    status: Severity::Pass,
                    checks: vec![CheckResult {
                        name: "Installed".to_string(),
                        detail: "yes".to_string(),
                        severity: Severity::Pass,
                        fix_hint: None,
                    }],
                },
            ],
        };
        print_report(&report);
    }
}
