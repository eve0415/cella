//! Orchestrator error types.

use miette::Diagnostic;

/// Errors from orchestrator operations.
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum OrchestratorError {
    #[error("backend: {message}")]
    #[diagnostic(code(cella::orchestrator::backend))]
    Backend { message: String },

    #[error("git: {message}")]
    #[diagnostic(code(cella::orchestrator::git))]
    Git { message: String },

    #[error("config: {message}")]
    #[diagnostic(code(cella::orchestrator::config))]
    Config { message: String },

    #[error("container exited immediately: {message}")]
    #[diagnostic(code(cella::orchestrator::container_exited))]
    ContainerExited { message: String },

    #[error("host requirements not met: {message}")]
    #[diagnostic(
        code(cella::host_requirements),
        help("Run `cella doctor` to check system dependencies.")
    )]
    HostRequirements { message: String },

    #[error("{message}")]
    #[diagnostic(code(cella::orchestrator::other))]
    Other { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_error_display() {
        let err = OrchestratorError::Backend {
            message: "connection refused".into(),
        };
        assert_eq!(err.to_string(), "backend: connection refused");
    }

    #[test]
    fn git_error_display() {
        let err = OrchestratorError::Git {
            message: "not a git repo".into(),
        };
        assert_eq!(err.to_string(), "git: not a git repo");
    }

    #[test]
    fn config_error_display() {
        let err = OrchestratorError::Config {
            message: "missing field".into(),
        };
        assert_eq!(err.to_string(), "config: missing field");
    }

    #[test]
    fn container_exited_display() {
        let err = OrchestratorError::ContainerExited {
            message: "exit code 1".into(),
        };
        assert_eq!(err.to_string(), "container exited immediately: exit code 1");
    }

    #[test]
    fn host_requirements_display() {
        let err = OrchestratorError::HostRequirements {
            message: "Docker not found".into(),
        };
        assert_eq!(
            err.to_string(),
            "host requirements not met: Docker not found"
        );
    }

    #[test]
    fn other_error_display() {
        let err = OrchestratorError::Other {
            message: "unexpected".into(),
        };
        assert_eq!(err.to_string(), "unexpected");
    }

    #[test]
    fn error_is_debug() {
        let err = OrchestratorError::Backend {
            message: "test".into(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("Backend"));
    }
}
