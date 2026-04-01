use thiserror::Error;

/// Errors that can occur during container runtime operations.
#[derive(Debug, Error)]
pub enum CellaDockerError {
    /// The container runtime is not available.
    #[error("container runtime not found: {message}")]
    RuntimeNotFound { message: String },

    /// Docker API error.
    #[error("Docker API error: {0}")]
    DockerApi(#[from] bollard::errors::Error),

    /// Image not found.
    #[error("image not found: {image}")]
    ImageNotFound { image: String },

    /// Docker CLI not found.
    #[error("docker CLI not found: {message}")]
    DockerCliNotFound { message: String },

    /// Image build failed.
    #[error("build failed: {message}")]
    BuildFailed { message: String },

    /// No container for the given workspace.
    #[error("container not found for workspace: {workspace}")]
    ContainerNotFound { workspace: String },

    /// Container exists but is not running.
    #[error("{hint}")]
    ContainerNotRunning { hint: String },

    /// A command executed inside the container failed.
    #[error("exec failed (exit code {exit_code}): {command}")]
    ExecFailed { command: String, exit_code: i64 },

    /// A lifecycle command phase failed.
    #[error("lifecycle command failed: {phase} — {message}")]
    LifecycleFailed { phase: String, message: String },

    /// A host-side command failed.
    #[error("host command failed: {command}")]
    HostCommandFailed {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// The container exited immediately after start.
    #[error("container exited immediately (exit code {exit_code}):\n{logs_tail}")]
    ContainerExitedImmediately { exit_code: i64, logs_tail: String },

    /// Agent volume population error.
    #[error("agent volume error: {message}")]
    AgentVolume { message: String },

    /// Agent binary checksum verification failed.
    #[error("agent binary checksum mismatch: expected {expected}, got {actual}")]
    AgentChecksumMismatch { expected: String, actual: String },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<CellaDockerError> for cella_backend::BackendError {
    fn from(e: CellaDockerError) -> Self {
        match e {
            CellaDockerError::RuntimeNotFound { message } => Self::ConnectionFailed { message },
            CellaDockerError::DockerApi(e) => Self::Runtime(Box::new(e)),
            CellaDockerError::ImageNotFound { image } => Self::ImageNotFound { image },
            CellaDockerError::DockerCliNotFound { message } => Self::CliNotFound { message },
            CellaDockerError::BuildFailed { message } => Self::ImageBuildFailed { message },
            CellaDockerError::ContainerNotFound { workspace } => Self::ContainerNotFound {
                identifier: workspace,
            },
            CellaDockerError::ContainerNotRunning { hint } => Self::ContainerNotRunning { hint },
            CellaDockerError::ExecFailed { command, exit_code } => {
                Self::ExecFailed { command, exit_code }
            }
            CellaDockerError::LifecycleFailed { phase, message } => {
                Self::LifecycleFailed { phase, message }
            }
            CellaDockerError::HostCommandFailed { command, source } => {
                Self::HostCommandFailed { command, source }
            }
            CellaDockerError::ContainerExitedImmediately {
                exit_code,
                logs_tail,
            } => Self::ContainerExitedImmediately {
                exit_code,
                logs_tail,
            },
            CellaDockerError::AgentVolume { message } => Self::AgentVolume { message },
            CellaDockerError::AgentChecksumMismatch { expected, actual } => {
                Self::AgentChecksumMismatch { expected, actual }
            }
            CellaDockerError::Io(e) => Self::Io(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cella_backend::BackendError;

    #[test]
    fn runtime_not_found_maps_to_connection_failed() {
        let err = CellaDockerError::RuntimeNotFound {
            message: "docker not installed".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::ConnectionFailed { message } if message == "docker not installed")
        );
    }

    #[test]
    fn image_not_found_maps_correctly() {
        let err = CellaDockerError::ImageNotFound {
            image: "ubuntu:latest".to_string(),
        };
        let be: BackendError = err.into();
        assert!(matches!(be, BackendError::ImageNotFound { image } if image == "ubuntu:latest"));
    }

    #[test]
    fn docker_cli_not_found_maps_to_cli_not_found() {
        let err = CellaDockerError::DockerCliNotFound {
            message: "not in PATH".to_string(),
        };
        let be: BackendError = err.into();
        assert!(matches!(be, BackendError::CliNotFound { message } if message == "not in PATH"));
    }

    #[test]
    fn build_failed_maps_to_image_build_failed() {
        let err = CellaDockerError::BuildFailed {
            message: "syntax error in Dockerfile".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::ImageBuildFailed { message } if message == "syntax error in Dockerfile")
        );
    }

    #[test]
    fn container_not_found_maps_workspace_to_identifier() {
        let err = CellaDockerError::ContainerNotFound {
            workspace: "/home/user/project".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::ContainerNotFound { identifier } if identifier == "/home/user/project")
        );
    }

    #[test]
    fn container_not_running_maps_correctly() {
        let err = CellaDockerError::ContainerNotRunning {
            hint: "run cella up first".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::ContainerNotRunning { hint } if hint == "run cella up first")
        );
    }

    #[test]
    fn exec_failed_maps_correctly() {
        let err = CellaDockerError::ExecFailed {
            command: "npm install".to_string(),
            exit_code: 1,
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::ExecFailed { command, exit_code } if command == "npm install" && exit_code == 1)
        );
    }

    #[test]
    fn lifecycle_failed_maps_correctly() {
        let err = CellaDockerError::LifecycleFailed {
            phase: "postCreate".to_string(),
            message: "script exited 127".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::LifecycleFailed { phase, message } if phase == "postCreate" && message == "script exited 127")
        );
    }

    #[test]
    fn host_command_failed_maps_correctly() {
        let err = CellaDockerError::HostCommandFailed {
            command: "docker compose".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::HostCommandFailed { command, .. } if command == "docker compose")
        );
    }

    #[test]
    fn container_exited_immediately_maps_correctly() {
        let err = CellaDockerError::ContainerExitedImmediately {
            exit_code: 137,
            logs_tail: "OOM killed".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::ContainerExitedImmediately { exit_code, logs_tail } if exit_code == 137 && logs_tail == "OOM killed")
        );
    }

    #[test]
    fn agent_volume_maps_correctly() {
        let err = CellaDockerError::AgentVolume {
            message: "volume missing".to_string(),
        };
        let be: BackendError = err.into();
        assert!(matches!(be, BackendError::AgentVolume { message } if message == "volume missing"));
    }

    #[test]
    fn agent_checksum_mismatch_maps_correctly() {
        let err = CellaDockerError::AgentChecksumMismatch {
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::AgentChecksumMismatch { expected, actual } if expected == "abc123" && actual == "def456")
        );
    }

    #[test]
    fn io_error_maps_correctly() {
        let err = CellaDockerError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let be: BackendError = err.into();
        assert!(
            matches!(be, BackendError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn docker_api_error_maps_to_runtime() {
        // bollard errors are opaque; just verify the variant is Runtime
        let bollard_err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "internal error".to_string(),
        };
        let err = CellaDockerError::DockerApi(bollard_err);
        let be: BackendError = err.into();
        assert!(matches!(be, BackendError::Runtime(_)));
    }

    #[test]
    fn error_display_messages() {
        let err = CellaDockerError::RuntimeNotFound {
            message: "daemon down".to_string(),
        };
        assert_eq!(err.to_string(), "container runtime not found: daemon down");

        let err = CellaDockerError::ExecFailed {
            command: "ls".to_string(),
            exit_code: 2,
        };
        assert_eq!(err.to_string(), "exec failed (exit code 2): ls");

        let err = CellaDockerError::AgentChecksumMismatch {
            expected: "aaa".to_string(),
            actual: "bbb".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "agent binary checksum mismatch: expected aaa, got bbb"
        );
    }

    // -----------------------------------------------------------------------
    // Display format tests for all remaining variants
    // -----------------------------------------------------------------------

    #[test]
    fn display_image_not_found() {
        let err = CellaDockerError::ImageNotFound {
            image: "myimage:latest".to_string(),
        };
        assert_eq!(err.to_string(), "image not found: myimage:latest");
    }

    #[test]
    fn display_docker_cli_not_found() {
        let err = CellaDockerError::DockerCliNotFound {
            message: "not in PATH".to_string(),
        };
        assert_eq!(err.to_string(), "docker CLI not found: not in PATH");
    }

    #[test]
    fn display_build_failed() {
        let err = CellaDockerError::BuildFailed {
            message: "Dockerfile syntax error".to_string(),
        };
        assert_eq!(err.to_string(), "build failed: Dockerfile syntax error");
    }

    #[test]
    fn display_container_not_found() {
        let err = CellaDockerError::ContainerNotFound {
            workspace: "/home/user/project".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "container not found for workspace: /home/user/project"
        );
    }

    #[test]
    fn display_container_not_running() {
        let err = CellaDockerError::ContainerNotRunning {
            hint: "Run cella up to start it.".to_string(),
        };
        assert_eq!(err.to_string(), "Run cella up to start it.");
    }

    #[test]
    fn display_lifecycle_failed() {
        let err = CellaDockerError::LifecycleFailed {
            phase: "postCreate".to_string(),
            message: "command exited 1".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "lifecycle command failed: postCreate — command exited 1"
        );
    }

    #[test]
    fn display_host_command_failed() {
        let err = CellaDockerError::HostCommandFailed {
            command: "docker compose up".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(err.to_string(), "host command failed: docker compose up");
    }

    #[test]
    fn display_container_exited_immediately() {
        let err = CellaDockerError::ContainerExitedImmediately {
            exit_code: 137,
            logs_tail: "OOM killed".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("137"));
        assert!(msg.contains("OOM killed"));
    }

    #[test]
    fn display_agent_volume() {
        let err = CellaDockerError::AgentVolume {
            message: "volume missing".to_string(),
        };
        assert_eq!(err.to_string(), "agent volume error: volume missing");
    }

    #[test]
    fn display_io_error() {
        let err = CellaDockerError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let msg = err.to_string();
        assert!(msg.contains("access denied"));
    }

    // -----------------------------------------------------------------------
    // From conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn from_io_error_into_cella_docker_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let err: CellaDockerError = io_err.into();
        assert!(matches!(err, CellaDockerError::Io(_)));
        assert!(err.to_string().contains("broken pipe"));
    }

    #[test]
    fn from_bollard_error_into_cella_docker_error() {
        let bollard_err = bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "no such container".to_string(),
        };
        let err: CellaDockerError = bollard_err.into();
        assert!(matches!(err, CellaDockerError::DockerApi(_)));
    }

    // -----------------------------------------------------------------------
    // Debug trait test
    // -----------------------------------------------------------------------

    #[test]
    fn error_debug_impl() {
        let err = CellaDockerError::RuntimeNotFound {
            message: "test".to_string(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("RuntimeNotFound"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn error_debug_all_variants() {
        let variants: Vec<CellaDockerError> = vec![
            CellaDockerError::ImageNotFound {
                image: "x".to_string(),
            },
            CellaDockerError::DockerCliNotFound {
                message: "x".to_string(),
            },
            CellaDockerError::BuildFailed {
                message: "x".to_string(),
            },
            CellaDockerError::ContainerNotFound {
                workspace: "x".to_string(),
            },
            CellaDockerError::ContainerNotRunning {
                hint: "x".to_string(),
            },
            CellaDockerError::ExecFailed {
                command: "x".to_string(),
                exit_code: 1,
            },
            CellaDockerError::LifecycleFailed {
                phase: "x".to_string(),
                message: "y".to_string(),
            },
            CellaDockerError::ContainerExitedImmediately {
                exit_code: 1,
                logs_tail: "x".to_string(),
            },
            CellaDockerError::AgentVolume {
                message: "x".to_string(),
            },
            CellaDockerError::AgentChecksumMismatch {
                expected: "a".to_string(),
                actual: "b".to_string(),
            },
        ];
        for err in variants {
            let debug = format!("{err:?}");
            assert!(!debug.is_empty());
        }
    }
}
