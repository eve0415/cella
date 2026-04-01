//! Git, GitHub CLI, and SSH agent checks.

use cella_env::gh_credential;

use super::{CategoryReport, CheckContext, CheckResult, Severity};

/// Run git and credential diagnostics.
pub async fn check_git(_ctx: &CheckContext) -> CategoryReport {
    let mut checks = Vec::new();

    // git in PATH
    match tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            checks.push(CheckResult {
                name: "git".into(),
                severity: Severity::Pass,
                detail: version,
                fix_hint: None,
            });
        }
        _ => {
            checks.push(CheckResult {
                name: "git".into(),
                severity: Severity::Error,
                detail: "not found in PATH".into(),
                fix_hint: Some("Install git: https://git-scm.com/downloads".into()),
            });
        }
    }

    // gh CLI
    let gh_status = gh_credential::probe_host_gh_status();
    if gh_status.installed {
        checks.push(CheckResult {
            name: "gh CLI".into(),
            severity: Severity::Pass,
            detail: "installed".into(),
            fix_hint: None,
        });

        if gh_status.authenticated {
            checks.push(CheckResult {
                name: "gh auth".into(),
                severity: Severity::Pass,
                detail: "authenticated".into(),
                fix_hint: None,
            });
        } else {
            checks.push(CheckResult {
                name: "gh auth".into(),
                severity: Severity::Warning,
                detail: "not authenticated".into(),
                fix_hint: Some("Run `gh auth login`".into()),
            });
        }
    } else {
        checks.push(CheckResult {
            name: "gh CLI".into(),
            severity: Severity::Warning,
            detail: "not installed".into(),
            fix_hint: Some("Install GitHub CLI: https://cli.github.com/".into()),
        });
    }

    // SSH agent
    check_ssh_agent(&mut checks);

    CategoryReport::new("Git & Credentials", checks)
}

fn check_ssh_agent(checks: &mut Vec<CheckResult>) {
    let runtime = cella_env::platform::detect_runtime();

    match std::env::var("SSH_AUTH_SOCK") {
        Ok(sock) if !sock.is_empty() => {
            let socket_type = classify_ssh_socket(&sock);
            let exists = std::path::Path::new(&sock).exists();
            if exists {
                checks.push(CheckResult {
                    name: "SSH agent".into(),
                    severity: Severity::Pass,
                    detail: format!("{sock} ({socket_type})"),
                    fix_hint: None,
                });
            } else {
                checks.push(CheckResult {
                    name: "SSH agent".into(),
                    severity: Severity::Warning,
                    detail: format!("{sock} (socket does not exist)"),
                    fix_hint: Some("Restart your SSH agent or check SSH_AUTH_SOCK".into()),
                });
            }
        }
        _ => {
            // On Docker Desktop/OrbStack, the runtime provides SSH forwarding
            if matches!(
                runtime,
                cella_env::DockerRuntime::DockerDesktop | cella_env::DockerRuntime::OrbStack
            ) {
                checks.push(CheckResult {
                    name: "SSH agent".into(),
                    severity: Severity::Pass,
                    detail: format!("provided by {runtime} runtime"),
                    fix_hint: None,
                });
            } else {
                checks.push(CheckResult {
                    name: "SSH agent".into(),
                    severity: Severity::Warning,
                    detail: "SSH_AUTH_SOCK not set".into(),
                    fix_hint: Some("Start ssh-agent: `eval $(ssh-agent -s) && ssh-add`".into()),
                });
            }
        }
    }
}

fn classify_ssh_socket(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.contains("orbstack") {
        "OrbStack"
    } else if lower.contains("docker-desktop") || lower.contains("com.apple.launchd") {
        "Docker Desktop VM"
    } else if lower.contains("colima") {
        "Colima"
    } else {
        "host-native"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ssh_socket_types() {
        assert_eq!(
            classify_ssh_socket("/run/orbstack/ssh-agent.sock"),
            "OrbStack"
        );
        assert_eq!(
            classify_ssh_socket("/var/run/docker-desktop/ssh-agent.sock"),
            "Docker Desktop VM"
        );
        assert_eq!(
            classify_ssh_socket("/Users/x/.colima/ssh-agent.sock"),
            "Colima"
        );
        assert_eq!(
            classify_ssh_socket("/tmp/ssh-XXXX/agent.12345"),
            "host-native"
        );
        assert_eq!(
            classify_ssh_socket("/private/tmp/com.apple.launchd.xxx/Listeners"),
            "Docker Desktop VM"
        );
    }

    #[test]
    fn classify_ssh_socket_case_insensitive() {
        assert_eq!(
            classify_ssh_socket("/run/OrbStack/SSH-Agent.sock"),
            "OrbStack"
        );
        assert_eq!(
            classify_ssh_socket("/var/run/Docker-Desktop/agent.sock"),
            "Docker Desktop VM"
        );
        assert_eq!(
            classify_ssh_socket("/Users/x/.Colima/default/ssh.sock"),
            "Colima"
        );
    }

    #[test]
    fn classify_ssh_socket_empty_string() {
        assert_eq!(classify_ssh_socket(""), "host-native");
    }

    #[test]
    fn classify_ssh_socket_windows_style_path() {
        assert_eq!(
            classify_ssh_socket(r"C:\Users\x\AppData\ssh-agent.sock"),
            "host-native"
        );
    }

    #[test]
    fn classify_ssh_socket_orbstack_variants() {
        assert_eq!(
            classify_ssh_socket("/var/run/orbstack/agent.sock"),
            "OrbStack"
        );
        assert_eq!(classify_ssh_socket("/tmp/ORBSTACK/ssh.sock"), "OrbStack");
    }

    #[test]
    fn classify_ssh_socket_colima_subpath() {
        assert_eq!(
            classify_ssh_socket("/Users/user/.colima/default/ssh-agent.sock"),
            "Colima"
        );
    }

    #[test]
    fn classify_ssh_socket_docker_desktop_various() {
        assert_eq!(
            classify_ssh_socket("/run/docker-desktop/ssh.sock"),
            "Docker Desktop VM"
        );
        assert_eq!(
            classify_ssh_socket("/private/tmp/com.apple.launchd.xyz/Listeners"),
            "Docker Desktop VM"
        );
    }

    #[test]
    fn classify_ssh_socket_generic_path() {
        assert_eq!(
            classify_ssh_socket("/run/user/1000/ssh-agent.sock"),
            "host-native"
        );
    }

    #[tokio::test]
    async fn check_git_returns_category_name() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            docker_client: None,
        };
        let report = check_git(&ctx).await;
        assert_eq!(report.name, "Git & Credentials");
    }

    #[tokio::test]
    async fn check_git_has_ssh_agent_check() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            docker_client: None,
        };
        let report = check_git(&ctx).await;
        let has_ssh = report.checks.iter().any(|c| c.name == "SSH agent");
        assert!(has_ssh, "should have SSH agent check");
    }

    #[tokio::test]
    async fn check_git_has_git_check() {
        let ctx = CheckContext {
            workspace_folder: None,
            all: false,
            docker_client: None,
        };
        let report = check_git(&ctx).await;
        let has_git = report.checks.iter().any(|c| c.name == "git");
        assert!(has_git, "should have git check");
    }
}
