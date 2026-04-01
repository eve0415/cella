//! Core types and orchestration for doctor checks.

pub mod config;
pub mod container;
pub mod daemon;
pub mod docker;
pub mod git;
pub mod system;

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use tokio::time::timeout;

use cella_docker::DockerClient;

/// Default timeout per check category.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Severity level for a check result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Check passed.
    Pass,
    /// Non-blocking issue.
    Warning,
    /// Blocking issue.
    Error,
    /// Informational (not pass/fail).
    Info,
}

/// Result of a single diagnostic check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Short name for the check.
    pub name: String,
    /// Outcome severity.
    #[serde(rename = "status")]
    pub severity: Severity,
    /// Human-readable detail.
    pub detail: String,
    /// Suggested fix command or action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

/// Results for one category of checks.
#[derive(Debug, Clone, Serialize)]
pub struct CategoryReport {
    /// Category display name.
    #[serde(skip)]
    pub name: String,
    /// Worst severity across all checks in this category.
    pub status: Severity,
    /// Individual check results.
    pub checks: Vec<CheckResult>,
}

impl CategoryReport {
    fn new(name: impl Into<String>, checks: Vec<CheckResult>) -> Self {
        let status = checks
            .iter()
            .map(|c| c.severity)
            .max_by_key(|s| match s {
                Severity::Error => 3,
                Severity::Warning => 2,
                Severity::Pass => 1,
                Severity::Info => 0,
            })
            .unwrap_or(Severity::Pass);
        Self {
            name: name.into(),
            status,
            checks,
        }
    }
}

/// Full diagnostic report across all categories.
#[derive(Debug, Clone)]
pub struct Report {
    pub categories: Vec<CategoryReport>,
}

impl Report {
    /// Whether any check produced an error.
    pub fn has_errors(&self) -> bool {
        self.categories
            .iter()
            .flat_map(|c| &c.checks)
            .any(|r| r.severity == Severity::Error)
    }

    /// Redact sensitive information from all check results.
    pub fn redact(&mut self, redactor: &crate::redact::Redactor) {
        for category in &mut self.categories {
            for check in &mut category.checks {
                check.detail = redactor.redact(&check.detail);
                if let Some(ref hint) = check.fix_hint {
                    check.fix_hint = Some(redactor.redact(hint));
                }
            }
        }
    }

    /// Overall status string.
    fn overall_status(&self) -> &'static str {
        if self.has_errors() {
            "error"
        } else if self
            .categories
            .iter()
            .flat_map(|c| &c.checks)
            .any(|r| r.severity == Severity::Warning)
        {
            "warn"
        } else {
            "ok"
        }
    }

    /// Serialize to JSON for machine-readable output.
    pub fn to_json(&self) -> serde_json::Value {
        let mut categories = serde_json::Map::new();
        for cat in &self.categories {
            let key = cat
                .name
                .to_lowercase()
                .replace(" & ", "_")
                .replace(' ', "_");
            categories.insert(key, serde_json::to_value(cat).unwrap_or_default());
        }
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "overall": self.overall_status(),
            "categories": categories,
        })
    }
}

/// Shared context for all checks.
pub struct CheckContext {
    /// Current workspace folder, if detected.
    pub workspace_folder: Option<PathBuf>,
    /// Whether to check all containers (--all flag).
    pub all: bool,
    /// Docker client, if connection succeeded.
    pub docker_client: Option<DockerClient>,
}

impl CheckContext {
    /// Build a new context, attempting Docker connection.
    pub fn new(workspace_folder: Option<PathBuf>, all: bool) -> Self {
        let docker_client = DockerClient::connect().ok();
        Self {
            workspace_folder,
            all,
            docker_client,
        }
    }
}

/// Run all diagnostic checks and produce a report.
pub async fn run_all_checks(ctx: &CheckContext) -> Report {
    let mut categories = Vec::new();

    // System info (no timeout needed, all local)
    categories.push(system::check_system(ctx).await);

    // Docker checks
    if let Ok(cat) = timeout(CHECK_TIMEOUT, docker::check_docker(ctx)).await {
        categories.push(cat);
    } else {
        categories.push(CategoryReport::new(
            "Docker",
            vec![CheckResult {
                name: "timeout".into(),
                severity: Severity::Error,
                detail: "Docker checks timed out after 5s".into(),
                fix_hint: Some("Docker daemon may be unresponsive".into()),
            }],
        ));
    }

    // Git & Credentials checks
    if let Ok(cat) = timeout(CHECK_TIMEOUT, git::check_git(ctx)).await {
        categories.push(cat);
    } else {
        categories.push(CategoryReport::new(
            "Git & Credentials",
            vec![CheckResult {
                name: "timeout".into(),
                severity: Severity::Warning,
                detail: "Git checks timed out after 5s".into(),
                fix_hint: None,
            }],
        ));
    }

    // Daemon checks
    let daemon_running;
    if let Ok(cat) = timeout(CHECK_TIMEOUT, daemon::check_daemon()).await {
        daemon_running = cat
            .checks
            .iter()
            .any(|c| c.name == "running" && c.severity == Severity::Pass);
        categories.push(cat);
    } else {
        daemon_running = false;
        categories.push(CategoryReport::new(
            "Daemon",
            vec![CheckResult {
                name: "timeout".into(),
                severity: Severity::Warning,
                detail: "Daemon checks timed out after 5s".into(),
                fix_hint: None,
            }],
        ));
    }

    // Configuration checks
    if let Ok(cat) = timeout(CHECK_TIMEOUT, async { config::check_config(ctx) }).await {
        categories.push(cat);
    } else {
        categories.push(CategoryReport::new(
            "Configuration",
            vec![CheckResult {
                name: "timeout".into(),
                severity: Severity::Warning,
                detail: "Configuration checks timed out after 5s".into(),
                fix_hint: None,
            }],
        ));
    }

    // Container checks
    if let Ok(mut cats) = timeout(
        CHECK_TIMEOUT,
        container::check_containers(ctx, daemon_running),
    )
    .await
    {
        categories.append(&mut cats);
    } else {
        categories.push(CategoryReport::new(
            "Containers",
            vec![CheckResult {
                name: "timeout".into(),
                severity: Severity::Warning,
                detail: "Container checks timed out after 5s".into(),
                fix_hint: None,
            }],
        ));
    }

    Report { categories }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_errors_with_error() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "check".into(),
                    severity: Severity::Error,
                    detail: "bad".into(),
                    fix_hint: None,
                }],
            )],
        };
        assert!(report.has_errors());
    }

    #[test]
    fn report_has_errors_without_error() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![
                    CheckResult {
                        name: "ok".into(),
                        severity: Severity::Pass,
                        detail: "good".into(),
                        fix_hint: None,
                    },
                    CheckResult {
                        name: "warn".into(),
                        severity: Severity::Warning,
                        detail: "meh".into(),
                        fix_hint: None,
                    },
                ],
            )],
        };
        assert!(!report.has_errors());
    }

    #[test]
    fn report_overall_status() {
        let ok_report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "ok".into(),
                    severity: Severity::Pass,
                    detail: "good".into(),
                    fix_hint: None,
                }],
            )],
        };
        assert_eq!(ok_report.overall_status(), "ok");

        let warn_report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "w".into(),
                    severity: Severity::Warning,
                    detail: "warn".into(),
                    fix_hint: None,
                }],
            )],
        };
        assert_eq!(warn_report.overall_status(), "warn");

        let err_report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "e".into(),
                    severity: Severity::Error,
                    detail: "err".into(),
                    fix_hint: None,
                }],
            )],
        };
        assert_eq!(err_report.overall_status(), "error");
    }

    #[test]
    fn report_to_json_structure() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "Docker",
                vec![CheckResult {
                    name: "daemon".into(),
                    severity: Severity::Pass,
                    detail: "running".into(),
                    fix_hint: None,
                }],
            )],
        };
        let json = report.to_json();
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["overall"], "ok");
        assert!(json["categories"]["docker"].is_object());
        assert_eq!(json["categories"]["docker"]["checks"][0]["name"], "daemon");
    }

    #[test]
    fn category_report_status_is_worst() {
        let cat = CategoryReport::new(
            "test",
            vec![
                CheckResult {
                    name: "a".into(),
                    severity: Severity::Pass,
                    detail: String::new(),
                    fix_hint: None,
                },
                CheckResult {
                    name: "b".into(),
                    severity: Severity::Warning,
                    detail: String::new(),
                    fix_hint: None,
                },
            ],
        );
        assert_eq!(cat.status, Severity::Warning);
    }

    #[test]
    fn category_report_empty_checks_defaults_to_pass() {
        let cat = CategoryReport::new("empty", Vec::new());
        assert_eq!(cat.status, Severity::Pass);
        assert!(cat.checks.is_empty());
    }

    #[test]
    fn category_report_error_dominates_warning() {
        let cat = CategoryReport::new(
            "test",
            vec![
                CheckResult {
                    name: "w".into(),
                    severity: Severity::Warning,
                    detail: String::new(),
                    fix_hint: None,
                },
                CheckResult {
                    name: "e".into(),
                    severity: Severity::Error,
                    detail: String::new(),
                    fix_hint: None,
                },
                CheckResult {
                    name: "p".into(),
                    severity: Severity::Pass,
                    detail: String::new(),
                    fix_hint: None,
                },
            ],
        );
        assert_eq!(cat.status, Severity::Error);
    }

    #[test]
    fn category_report_info_only() {
        let cat = CategoryReport::new(
            "info",
            vec![CheckResult {
                name: "i".into(),
                severity: Severity::Info,
                detail: "informational".into(),
                fix_hint: None,
            }],
        );
        assert_eq!(cat.status, Severity::Info);
    }

    #[test]
    fn report_has_errors_empty_categories() {
        let report = Report {
            categories: Vec::new(),
        };
        assert!(!report.has_errors());
    }

    #[test]
    fn report_overall_status_empty() {
        let report = Report {
            categories: Vec::new(),
        };
        assert_eq!(report.overall_status(), "ok");
    }

    #[test]
    fn report_overall_info_only_is_ok() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "i".into(),
                    severity: Severity::Info,
                    detail: "info".into(),
                    fix_hint: None,
                }],
            )],
        };
        assert_eq!(report.overall_status(), "ok");
    }

    #[test]
    fn report_redact_redacts_detail_and_fix_hint() {
        let redactor = crate::redact::Redactor::new();
        let mut report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "token".into(),
                    severity: Severity::Info,
                    detail: "Token gho_abcdef123456".into(),
                    fix_hint: Some("Check gho_abcdef123456".into()),
                }],
            )],
        };
        report.redact(&redactor);
        assert!(
            report.categories[0].checks[0].detail.contains("<redacted>"),
            "detail should be redacted"
        );
        assert!(
            report.categories[0].checks[0]
                .fix_hint
                .as_ref()
                .unwrap()
                .contains("<redacted>"),
            "fix_hint should be redacted"
        );
    }

    #[test]
    fn report_to_json_category_key_normalization() {
        let report = Report {
            categories: vec![
                CategoryReport::new(
                    "Git & Credentials",
                    vec![CheckResult {
                        name: "git".into(),
                        severity: Severity::Pass,
                        detail: "ok".into(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "System Info",
                    vec![CheckResult {
                        name: "cella".into(),
                        severity: Severity::Info,
                        detail: "1.0.0".into(),
                        fix_hint: None,
                    }],
                ),
            ],
        };
        let json = report.to_json();
        // "Git & Credentials" -> "git_credentials"
        assert!(
            json["categories"]["git_credentials"].is_object(),
            "expected 'git_credentials' key, got: {json:?}"
        );
        // "System Info" -> "system_info"
        assert!(
            json["categories"]["system_info"].is_object(),
            "expected 'system_info' key"
        );
    }

    #[test]
    fn report_to_json_has_version_and_overall() {
        let report = Report {
            categories: Vec::new(),
        };
        let json = report.to_json();
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["overall"], "ok");
        assert!(json["categories"].is_object());
    }

    #[test]
    fn check_result_serializes_severity_as_status() {
        let check = CheckResult {
            name: "test".into(),
            severity: Severity::Pass,
            detail: "ok".into(),
            fix_hint: None,
        };
        let json = serde_json::to_value(&check).unwrap();
        assert_eq!(json["status"], "pass");
        assert!(
            json.get("fix_hint").is_none(),
            "None fix_hint should be skipped"
        );
    }

    #[test]
    fn check_result_serializes_fix_hint_when_present() {
        let check = CheckResult {
            name: "test".into(),
            severity: Severity::Warning,
            detail: "issue".into(),
            fix_hint: Some("do this".into()),
        };
        let json = serde_json::to_value(&check).unwrap();
        assert_eq!(json["fix_hint"], "do this");
        assert_eq!(json["status"], "warning");
    }

    #[test]
    fn severity_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Severity::Pass).unwrap(), "\"pass\"");
        assert_eq!(
            serde_json::to_string(&Severity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Error).unwrap(),
            "\"error\""
        );
        assert_eq!(serde_json::to_string(&Severity::Info).unwrap(), "\"info\"");
    }

    // --- Additional CategoryReport tests ---

    #[test]
    fn category_report_single_pass() {
        let cat = CategoryReport::new(
            "test",
            vec![CheckResult {
                name: "a".into(),
                severity: Severity::Pass,
                detail: "ok".into(),
                fix_hint: None,
            }],
        );
        assert_eq!(cat.status, Severity::Pass);
        assert_eq!(cat.name, "test");
    }

    #[test]
    fn category_report_single_error() {
        let cat = CategoryReport::new(
            "test",
            vec![CheckResult {
                name: "a".into(),
                severity: Severity::Error,
                detail: "bad".into(),
                fix_hint: None,
            }],
        );
        assert_eq!(cat.status, Severity::Error);
    }

    #[test]
    fn category_report_info_does_not_dominate_pass() {
        let cat = CategoryReport::new(
            "mixed",
            vec![
                CheckResult {
                    name: "a".into(),
                    severity: Severity::Pass,
                    detail: String::new(),
                    fix_hint: None,
                },
                CheckResult {
                    name: "b".into(),
                    severity: Severity::Info,
                    detail: String::new(),
                    fix_hint: None,
                },
            ],
        );
        // Pass (1) > Info (0), so status should be Pass
        assert_eq!(cat.status, Severity::Pass);
    }

    #[test]
    fn category_report_warning_dominates_pass_and_info() {
        let cat = CategoryReport::new(
            "test",
            vec![
                CheckResult {
                    name: "p".into(),
                    severity: Severity::Pass,
                    detail: String::new(),
                    fix_hint: None,
                },
                CheckResult {
                    name: "i".into(),
                    severity: Severity::Info,
                    detail: String::new(),
                    fix_hint: None,
                },
                CheckResult {
                    name: "w".into(),
                    severity: Severity::Warning,
                    detail: String::new(),
                    fix_hint: None,
                },
            ],
        );
        assert_eq!(cat.status, Severity::Warning);
    }

    #[test]
    fn category_report_name_preserved() {
        let cat = CategoryReport::new("My Custom Category", Vec::new());
        assert_eq!(cat.name, "My Custom Category");
    }

    // --- Additional Report tests ---

    #[test]
    fn report_has_errors_multiple_categories_mixed() {
        let report = Report {
            categories: vec![
                CategoryReport::new(
                    "clean",
                    vec![CheckResult {
                        name: "ok".into(),
                        severity: Severity::Pass,
                        detail: "fine".into(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "broken",
                    vec![CheckResult {
                        name: "err".into(),
                        severity: Severity::Error,
                        detail: "broken".into(),
                        fix_hint: None,
                    }],
                ),
            ],
        };
        assert!(report.has_errors());
    }

    #[test]
    fn report_has_errors_multiple_categories_all_ok() {
        let report = Report {
            categories: vec![
                CategoryReport::new(
                    "a",
                    vec![CheckResult {
                        name: "ok".into(),
                        severity: Severity::Pass,
                        detail: "fine".into(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "b",
                    vec![CheckResult {
                        name: "ok".into(),
                        severity: Severity::Pass,
                        detail: "fine".into(),
                        fix_hint: None,
                    }],
                ),
            ],
        };
        assert!(!report.has_errors());
    }

    #[test]
    fn report_overall_status_warn_across_categories() {
        let report = Report {
            categories: vec![
                CategoryReport::new(
                    "a",
                    vec![CheckResult {
                        name: "ok".into(),
                        severity: Severity::Pass,
                        detail: "fine".into(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "b",
                    vec![CheckResult {
                        name: "w".into(),
                        severity: Severity::Warning,
                        detail: "issue".into(),
                        fix_hint: None,
                    }],
                ),
            ],
        };
        assert_eq!(report.overall_status(), "warn");
    }

    #[test]
    fn report_overall_status_error_dominates_warning() {
        let report = Report {
            categories: vec![
                CategoryReport::new(
                    "a",
                    vec![CheckResult {
                        name: "w".into(),
                        severity: Severity::Warning,
                        detail: String::new(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "b",
                    vec![CheckResult {
                        name: "e".into(),
                        severity: Severity::Error,
                        detail: String::new(),
                        fix_hint: None,
                    }],
                ),
            ],
        };
        assert_eq!(report.overall_status(), "error");
    }

    #[test]
    fn report_redact_with_no_fix_hint() {
        let redactor = crate::redact::Redactor::new();
        let mut report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "check".into(),
                    severity: Severity::Pass,
                    detail: "Token gho_secret123".into(),
                    fix_hint: None,
                }],
            )],
        };
        report.redact(&redactor);
        assert!(report.categories[0].checks[0].detail.contains("<redacted>"));
        assert!(report.categories[0].checks[0].fix_hint.is_none());
    }

    #[test]
    fn report_redact_multiple_categories() {
        let redactor = crate::redact::Redactor::new();
        let mut report = Report {
            categories: vec![
                CategoryReport::new(
                    "a",
                    vec![CheckResult {
                        name: "t1".into(),
                        severity: Severity::Info,
                        detail: "ghp_token1".into(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "b",
                    vec![CheckResult {
                        name: "t2".into(),
                        severity: Severity::Info,
                        detail: "ghs_token2".into(),
                        fix_hint: Some("use gho_hint123".into()),
                    }],
                ),
            ],
        };
        report.redact(&redactor);
        assert_eq!(report.categories[0].checks[0].detail, "<redacted>");
        assert_eq!(report.categories[1].checks[0].detail, "<redacted>");
        assert_eq!(
            report.categories[1].checks[0].fix_hint.as_deref(),
            Some("use <redacted>")
        );
    }

    #[test]
    fn report_to_json_multiple_categories() {
        let report = Report {
            categories: vec![
                CategoryReport::new(
                    "Docker",
                    vec![CheckResult {
                        name: "daemon".into(),
                        severity: Severity::Pass,
                        detail: "ok".into(),
                        fix_hint: None,
                    }],
                ),
                CategoryReport::new(
                    "Daemon",
                    vec![CheckResult {
                        name: "running".into(),
                        severity: Severity::Warning,
                        detail: "not running".into(),
                        fix_hint: Some("start it".into()),
                    }],
                ),
            ],
        };
        let json = report.to_json();
        assert!(json["categories"]["docker"].is_object());
        assert!(json["categories"]["daemon"].is_object());
        assert_eq!(json["overall"], "warn");
    }

    #[test]
    fn report_to_json_error_overall() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "Docker",
                vec![CheckResult {
                    name: "daemon".into(),
                    severity: Severity::Error,
                    detail: "down".into(),
                    fix_hint: None,
                }],
            )],
        };
        let json = report.to_json();
        assert_eq!(json["overall"], "error");
    }

    #[test]
    fn report_to_json_fix_hint_included_when_present() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "c".into(),
                    severity: Severity::Warning,
                    detail: "issue".into(),
                    fix_hint: Some("fix it".into()),
                }],
            )],
        };
        let json = report.to_json();
        assert_eq!(
            json["categories"]["test"]["checks"][0]["fix_hint"],
            "fix it"
        );
    }

    #[test]
    fn report_to_json_fix_hint_absent_when_none() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "test",
                vec![CheckResult {
                    name: "c".into(),
                    severity: Severity::Pass,
                    detail: "ok".into(),
                    fix_hint: None,
                }],
            )],
        };
        let json = report.to_json();
        assert!(
            json["categories"]["test"]["checks"][0]
                .get("fix_hint")
                .is_none(),
            "fix_hint should be absent when None"
        );
    }

    // --- Severity trait tests ---

    #[test]
    fn severity_copy() {
        let s = Severity::Pass;
        let copied = s;
        let copied2 = s;
        assert_eq!(copied, copied2);
    }

    #[test]
    fn severity_debug_format() {
        assert_eq!(format!("{:?}", Severity::Pass), "Pass");
        assert_eq!(format!("{:?}", Severity::Warning), "Warning");
        assert_eq!(format!("{:?}", Severity::Error), "Error");
        assert_eq!(format!("{:?}", Severity::Info), "Info");
    }

    #[test]
    fn severity_equality() {
        assert_eq!(Severity::Pass, Severity::Pass);
        assert_ne!(Severity::Pass, Severity::Warning);
        assert_ne!(Severity::Warning, Severity::Error);
        assert_ne!(Severity::Error, Severity::Info);
    }

    // --- CheckResult serialization ---

    #[test]
    fn check_result_all_severities_serialize() {
        for (severity, expected) in [
            (Severity::Pass, "pass"),
            (Severity::Warning, "warning"),
            (Severity::Error, "error"),
            (Severity::Info, "info"),
        ] {
            let check = CheckResult {
                name: "test".into(),
                severity,
                detail: "detail".into(),
                fix_hint: None,
            };
            let json = serde_json::to_value(&check).unwrap();
            assert_eq!(json["status"], expected);
        }
    }

    #[test]
    fn category_report_serializes_status_and_checks() {
        let cat = CategoryReport::new(
            "Docker",
            vec![CheckResult {
                name: "daemon".into(),
                severity: Severity::Pass,
                detail: "ok".into(),
                fix_hint: None,
            }],
        );
        let json = serde_json::to_value(&cat).unwrap();
        assert_eq!(json["status"], "pass");
        assert!(json["checks"].is_array());
        assert_eq!(json["checks"][0]["name"], "daemon");
        // name is #[serde(skip)], so it should not appear in JSON
        assert!(json.get("name").is_none());
    }

    // --- to_json key normalization edge cases ---

    #[test]
    fn report_to_json_key_with_multiple_spaces() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "Some Long Name",
                vec![CheckResult {
                    name: "c".into(),
                    severity: Severity::Pass,
                    detail: "ok".into(),
                    fix_hint: None,
                }],
            )],
        };
        let json = report.to_json();
        assert!(json["categories"]["some_long_name"].is_object());
    }

    #[test]
    fn report_to_json_key_with_ampersand_and_spaces() {
        let report = Report {
            categories: vec![CategoryReport::new(
                "A & B",
                vec![CheckResult {
                    name: "c".into(),
                    severity: Severity::Pass,
                    detail: "ok".into(),
                    fix_hint: None,
                }],
            )],
        };
        let json = report.to_json();
        // "A & B" -> lowercase "a & b" -> replace " & " -> "a_b"
        assert!(json["categories"]["a_b"].is_object(), "got: {json:?}");
    }
}
