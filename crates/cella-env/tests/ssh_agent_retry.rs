#![cfg(feature = "integration-tests")]

use cella_env::platform::DockerRuntime;
use cella_env::ssh_agent::{is_ssh_mount_error, ssh_skip_warning};

#[test]
fn realistic_docker_400_error_detected() {
    let docker_error = r#"Docker responded with status code 400: {"message":"invalid mount config for type \"bind\": bind source path does not exist: /run/host-services/ssh-auth.sock"}"#;
    assert!(is_ssh_mount_error(
        docker_error,
        Some("/run/host-services/ssh-auth.sock")
    ));
}

#[test]
fn realistic_docker_error_launchd_path() {
    let docker_error = r#"Docker responded with status code 400: {"message":"invalid mount config for type \"bind\": bind source path does not exist: /var/run/com.apple.launchd.hxQo6EKt4v/Listeners"}"#;
    assert!(is_ssh_mount_error(
        docker_error,
        Some("/var/run/com.apple.launchd.hxQo6EKt4v/Listeners")
    ));
}

#[test]
fn all_runtimes_have_warnings() {
    for runtime in [
        DockerRuntime::DockerDesktop,
        DockerRuntime::OrbStack,
        DockerRuntime::LinuxNative,
        DockerRuntime::Colima,
        DockerRuntime::Podman,
        DockerRuntime::RancherDesktop,
        DockerRuntime::Unknown,
    ] {
        let warning = ssh_skip_warning(&runtime);
        assert!(
            warning.contains("SSH agent forwarding skipped"),
            "runtime {runtime:?} missing base warning"
        );
    }
}

#[test]
fn rancher_desktop_warning_is_actionable() {
    let warning = ssh_skip_warning(&DockerRuntime::RancherDesktop);
    assert!(warning.contains("forwardAgent"));
    assert!(warning.contains("override.yaml"));
}
