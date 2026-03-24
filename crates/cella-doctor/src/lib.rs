//! System diagnostics and health checking for cella.
//!
//! Provides structured health checks for Docker, git, credentials,
//! daemon status, configuration, and running containers.

pub mod checks;
pub mod host_requirements;
pub mod redact;
