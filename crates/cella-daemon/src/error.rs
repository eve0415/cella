use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur in the cella daemon.
#[derive(Debug, Error, Diagnostic)]
pub enum CellaDaemonError {
    /// Failed to create or bind a socket.
    #[error("socket error: {message}")]
    #[diagnostic(code(cella::daemon::socket))]
    Socket { message: String },

    /// Failed to write or read the PID file.
    #[error("PID file error: {message}")]
    #[diagnostic(code(cella::daemon::pid_file))]
    PidFile { message: String },

    /// Failed to invoke the host git credential helper.
    #[error("git credential error: {message}")]
    #[diagnostic(code(cella::daemon::git_credential))]
    GitCredential { message: String },

    /// Protocol parse error.
    #[error("protocol error: {message}")]
    #[diagnostic(code(cella::daemon::protocol))]
    Protocol { message: String },

    /// The daemon is already running.
    #[error("cella daemon is already running (PID {pid})")]
    #[diagnostic(code(cella::daemon::already_running))]
    AlreadyRunning { pid: u32 },

    /// The daemon is not running.
    #[error("cella daemon is not running")]
    #[diagnostic(code(cella::daemon::not_running))]
    NotRunning,

    /// Port forwarding error.
    #[error("port forwarding error: {message}")]
    #[diagnostic(code(cella::daemon::port_forwarding))]
    PortForwarding { message: String },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    #[diagnostic(code(cella::daemon::io))]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Display output for every variant --

    #[test]
    fn display_socket_error() {
        let err = CellaDaemonError::Socket {
            message: "connection refused".to_string(),
        };
        assert_eq!(err.to_string(), "socket error: connection refused");
    }

    #[test]
    fn display_pid_file_error() {
        let err = CellaDaemonError::PidFile {
            message: "write failed".to_string(),
        };
        assert_eq!(err.to_string(), "PID file error: write failed");
    }

    #[test]
    fn display_git_credential_error() {
        let err = CellaDaemonError::GitCredential {
            message: "helper not found".to_string(),
        };
        assert_eq!(err.to_string(), "git credential error: helper not found");
    }

    #[test]
    fn display_protocol_error() {
        let err = CellaDaemonError::Protocol {
            message: "invalid JSON".to_string(),
        };
        assert_eq!(err.to_string(), "protocol error: invalid JSON");
    }

    #[test]
    fn display_already_running() {
        let err = CellaDaemonError::AlreadyRunning { pid: 42 };
        assert_eq!(err.to_string(), "cella daemon is already running (PID 42)");
    }

    #[test]
    fn display_not_running() {
        let err = CellaDaemonError::NotRunning;
        assert_eq!(err.to_string(), "cella daemon is not running");
    }

    #[test]
    fn display_port_forwarding_error() {
        let err = CellaDaemonError::PortForwarding {
            message: "bind failed".to_string(),
        };
        assert_eq!(err.to_string(), "port forwarding error: bind failed");
    }

    #[test]
    fn display_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = CellaDaemonError::Io(io_err);
        assert_eq!(err.to_string(), "I/O error: file missing");
    }

    // -- From conversion --

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: CellaDaemonError = io_err.into();
        assert!(matches!(err, CellaDaemonError::Io(_)));
        assert!(err.to_string().contains("access denied"));
    }

    // -- Debug is implemented --

    #[test]
    fn debug_format_works() {
        let err = CellaDaemonError::NotRunning;
        let debug = format!("{err:?}");
        assert!(debug.contains("NotRunning"));
    }

    #[test]
    fn debug_format_socket() {
        let err = CellaDaemonError::Socket {
            message: "test".to_string(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("Socket"));
        assert!(debug.contains("test"));
    }

    // -- Display with empty messages --

    #[test]
    fn display_socket_empty_message() {
        let err = CellaDaemonError::Socket {
            message: String::new(),
        };
        assert_eq!(err.to_string(), "socket error: ");
    }

    #[test]
    fn display_already_running_zero_pid() {
        let err = CellaDaemonError::AlreadyRunning { pid: 0 };
        assert_eq!(err.to_string(), "cella daemon is already running (PID 0)");
    }

    #[test]
    fn display_already_running_large_pid() {
        let err = CellaDaemonError::AlreadyRunning { pid: u32::MAX };
        assert_eq!(
            err.to_string(),
            format!("cella daemon is already running (PID {})", u32::MAX)
        );
    }

    // -- std::error::Error source chain --

    #[test]
    fn io_error_source_chain() {
        use std::error::Error;
        let io_err = std::io::Error::other("inner");
        let err = CellaDaemonError::Io(io_err);
        // The source should be the original io::Error
        assert!(err.source().is_some());
    }

    #[test]
    fn non_io_variants_have_no_source() {
        use std::error::Error;
        let err = CellaDaemonError::NotRunning;
        assert!(err.source().is_none());

        let err2 = CellaDaemonError::Socket {
            message: "x".into(),
        };
        assert!(err2.source().is_none());
    }
}
