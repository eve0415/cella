use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur during agent sandbox operations.
#[derive(Debug, Error, Diagnostic)]
pub enum CellaAgentError {
    /// Failed to create the agent sandbox.
    #[error("failed to create agent sandbox: {0}")]
    #[diagnostic(code(cella::agent::sandbox_creation))]
    SandboxCreation(String),

    /// The agent sandbox is not running.
    #[error("agent sandbox is not running")]
    #[diagnostic(code(cella::agent::not_running))]
    NotRunning,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_sandbox_creation() {
        let err = CellaAgentError::SandboxCreation("permission denied".to_string());
        let msg = err.to_string();
        assert_eq!(msg, "failed to create agent sandbox: permission denied");
    }

    #[test]
    fn display_sandbox_creation_empty_message() {
        let err = CellaAgentError::SandboxCreation(String::new());
        let msg = err.to_string();
        assert_eq!(msg, "failed to create agent sandbox: ");
    }

    #[test]
    fn display_not_running() {
        let err = CellaAgentError::NotRunning;
        assert_eq!(err.to_string(), "agent sandbox is not running");
    }

    #[test]
    fn debug_sandbox_creation() {
        let err = CellaAgentError::SandboxCreation("test".to_string());
        let dbg = format!("{err:?}");
        assert!(dbg.contains("SandboxCreation"));
        assert!(dbg.contains("test"));
    }

    #[test]
    fn debug_not_running() {
        let err = CellaAgentError::NotRunning;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("NotRunning"));
    }

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CellaAgentError>();
    }

    #[test]
    fn error_trait_source_is_none() {
        use std::error::Error;
        let err = CellaAgentError::NotRunning;
        assert!(err.source().is_none());

        let err = CellaAgentError::SandboxCreation("x".to_string());
        assert!(err.source().is_none());
    }
}
