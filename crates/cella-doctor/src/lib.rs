//! System diagnostics and health checking for cella.
//!
//! Provides structured health checks for Docker, git, credentials,
//! daemon status, configuration, and running containers.

pub mod checks;
/// Re-export from `cella_orchestrator` for backward compatibility.
pub use cella_orchestrator::host_requirements;
pub mod redact;
