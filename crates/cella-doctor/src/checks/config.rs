//! Devcontainer configuration checks.

use cella_config::devcontainer::discover;
use cella_config::devcontainer::parse;

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Run configuration diagnostics.
#[allow(clippy::unused_async)]
pub async fn check_config(ctx: &CheckContext) -> CategoryReport {
    let mut checks = Vec::new();

    let Some(ref workspace) = ctx.workspace_folder else {
        checks.push(CheckResult {
            name: "workspace".into(),
            severity: Severity::Info,
            detail: "no workspace directory detected".into(),
            fix_hint: None,
        });
        return CategoryReport::new("Configuration", checks);
    };

    // Discover devcontainer.json
    match discover::config(workspace) {
        Ok(config_path) => {
            checks.push(CheckResult {
                name: "devcontainer.json".into(),
                severity: Severity::Pass,
                detail: config_path.display().to_string(),
                fix_hint: None,
            });

            // Parse and validate
            match std::fs::read_to_string(&config_path) {
                Ok(raw_text) => {
                    let source_name = config_path.display().to_string();
                    match parse::devcontainer(&source_name, &raw_text, false) {
                        Ok((_parsed, warnings)) => {
                            if warnings.is_empty() {
                                checks.push(CheckResult {
                                    name: "config valid".into(),
                                    severity: Severity::Pass,
                                    detail: "parsed successfully".into(),
                                    fix_hint: None,
                                });
                            } else {
                                checks.push(CheckResult {
                                    name: "config valid".into(),
                                    severity: Severity::Warning,
                                    detail: format!("parsed with {} warning(s)", warnings.len()),
                                    fix_hint: Some(
                                        "Run `cella config validate` for details".into(),
                                    ),
                                });
                            }
                        }
                        Err(diagnostics) => {
                            checks.push(CheckResult {
                                name: "config valid".into(),
                                severity: Severity::Error,
                                detail: format!(
                                    "parse failed: {} error(s)",
                                    diagnostics.error_count()
                                ),
                                fix_hint: Some("Run `cella config validate` for details".into()),
                            });
                        }
                    }
                }
                Err(e) => {
                    checks.push(CheckResult {
                        name: "config valid".into(),
                        severity: Severity::Error,
                        detail: format!("could not read file: {e}"),
                        fix_hint: None,
                    });
                }
            }
        }
        Err(discover::Error::NotFound) => {
            checks.push(CheckResult {
                name: "devcontainer.json".into(),
                severity: Severity::Info,
                detail: "not found (not required)".into(),
                fix_hint: Some("Run `cella init` to create one".into()),
            });
        }
        Err(discover::Error::Ambiguous(paths)) => {
            let names: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
            checks.push(CheckResult {
                name: "devcontainer.json".into(),
                severity: Severity::Warning,
                detail: format!("multiple configs found: {}", names.join(", ")),
                fix_hint: Some("Use --file to specify which config to use".into()),
            });
        }
        Err(discover::Error::ReadDir { path, source }) => {
            checks.push(CheckResult {
                name: "devcontainer.json".into(),
                severity: Severity::Error,
                detail: format!("could not read {}: {source}", path.display()),
                fix_hint: None,
            });
        }
    }

    CategoryReport::new("Configuration", checks)
}
