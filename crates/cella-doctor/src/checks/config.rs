//! Devcontainer configuration checks.

use cella_config::devcontainer::discover;
use cella_config::devcontainer::parse;

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Run configuration diagnostics.
pub fn check_config(ctx: &CheckContext) -> CategoryReport {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_workspace(path: Option<std::path::PathBuf>) -> CheckContext {
        CheckContext {
            workspace_folder: path,
            all: false,
            docker_client: None,
        }
    }

    #[test]
    fn no_workspace_returns_info() {
        let ctx = ctx_with_workspace(None);
        let report = check_config(&ctx);
        assert_eq!(report.name, "Configuration");
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].name, "workspace");
        assert_eq!(report.checks[0].severity, Severity::Info);
        assert!(report.checks[0].detail.contains("no workspace"));
    }

    #[test]
    fn workspace_without_devcontainer_json_returns_info() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);

        // Should get a "not found" info result
        let config_check = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .expect("should have devcontainer.json check");
        assert_eq!(config_check.severity, Severity::Info);
        assert!(config_check.detail.contains("not found"));
        assert!(config_check.fix_hint.is_some());
    }

    #[test]
    fn workspace_with_valid_devcontainer_json_returns_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{ "name": "test", "image": "ubuntu" }"#,
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);

        let discovery_check = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .expect("should have devcontainer.json check");
        assert_eq!(discovery_check.severity, Severity::Pass);

        let valid_check = report
            .checks
            .iter()
            .find(|c| c.name == "config valid")
            .expect("should have config valid check");
        assert_eq!(valid_check.severity, Severity::Pass);
        assert!(valid_check.detail.contains("parsed successfully"));
    }

    #[test]
    fn workspace_with_invalid_json_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "{ this is not valid json !!!",
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);

        let valid_check = report
            .checks
            .iter()
            .find(|c| c.name == "config valid")
            .expect("should have config valid check");
        assert_eq!(valid_check.severity, Severity::Error);
        assert!(valid_check.detail.contains("parse failed"));
    }

    #[test]
    fn workspace_with_ambiguous_configs_returns_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");

        // Create two subfolder configs to trigger Ambiguous
        let sub_a = dc_dir.join("alpha");
        let sub_b = dc_dir.join("beta");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        std::fs::write(
            sub_a.join("devcontainer.json"),
            r#"{ "name": "a", "image": "ubuntu" }"#,
        )
        .unwrap();
        std::fs::write(
            sub_b.join("devcontainer.json"),
            r#"{ "name": "b", "image": "ubuntu" }"#,
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);

        let config_check = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .expect("should have devcontainer.json check");
        assert_eq!(config_check.severity, Severity::Warning);
        assert!(config_check.detail.contains("multiple configs found"));
    }

    #[test]
    fn no_workspace_has_no_fix_hint() {
        let ctx = ctx_with_workspace(None);
        let report = check_config(&ctx);
        assert!(report.checks[0].fix_hint.is_none());
    }

    #[test]
    fn no_workspace_category_name_is_configuration() {
        let ctx = ctx_with_workspace(None);
        let report = check_config(&ctx);
        assert_eq!(report.name, "Configuration");
    }

    #[test]
    fn workspace_without_devcontainer_has_fix_hint_for_init() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);
        let config_check = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .unwrap();
        assert!(
            config_check
                .fix_hint
                .as_ref()
                .unwrap()
                .contains("cella init")
        );
    }

    #[test]
    fn workspace_with_valid_config_has_two_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{ "name": "test", "image": "ubuntu" }"#,
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);
        assert!(
            report.checks.len() >= 2,
            "valid config should produce at least 2 checks (discovery + validation)"
        );
    }

    #[test]
    fn ambiguous_configs_has_fix_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        let sub_a = dc_dir.join("alpha");
        let sub_b = dc_dir.join("beta");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        std::fs::write(
            sub_a.join("devcontainer.json"),
            r#"{ "name": "a", "image": "ubuntu" }"#,
        )
        .unwrap();
        std::fs::write(
            sub_b.join("devcontainer.json"),
            r#"{ "name": "b", "image": "ubuntu" }"#,
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);
        let config_check = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .unwrap();
        assert!(config_check.fix_hint.is_some());
        assert!(config_check.fix_hint.as_ref().unwrap().contains("--file"));
    }

    #[test]
    fn invalid_json_has_fix_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            "{ this is not valid json !!!",
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);
        let valid_check = report
            .checks
            .iter()
            .find(|c| c.name == "config valid")
            .unwrap();
        assert!(valid_check.fix_hint.is_some());
        assert!(
            valid_check
                .fix_hint
                .as_ref()
                .unwrap()
                .contains("cella config validate")
        );
    }

    #[test]
    fn valid_config_discovery_shows_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        let config_file = dc_dir.join("devcontainer.json");
        std::fs::write(&config_file, r#"{ "name": "test", "image": "ubuntu" }"#).unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);
        let discovery = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .unwrap();
        assert!(
            discovery.detail.contains("devcontainer.json"),
            "discovery detail should contain the filename"
        );
    }

    #[test]
    fn valid_config_no_fix_hints_on_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let dc_dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc_dir).unwrap();
        std::fs::write(
            dc_dir.join("devcontainer.json"),
            r#"{ "name": "test", "image": "ubuntu" }"#,
        )
        .unwrap();

        let ctx = ctx_with_workspace(Some(tmp.path().to_path_buf()));
        let report = check_config(&ctx);
        for check in &report.checks {
            if check.severity == Severity::Pass {
                assert!(
                    check.fix_hint.is_none(),
                    "passing check '{}' should not have a fix_hint",
                    check.name
                );
            }
        }
    }

    #[test]
    fn nonexistent_workspace_dir_returns_error_or_not_found() {
        let ctx = ctx_with_workspace(Some(std::path::PathBuf::from(
            "/nonexistent/workspace/path/that/does/not/exist",
        )));
        let report = check_config(&ctx);
        // Should get either NotFound (Info) or ReadDir error
        let config_check = report
            .checks
            .iter()
            .find(|c| c.name == "devcontainer.json")
            .expect("should have devcontainer.json check");
        assert!(
            config_check.severity == Severity::Info || config_check.severity == Severity::Error,
            "expected Info or Error for nonexistent workspace, got {:?}",
            config_check.severity
        );
    }
}
