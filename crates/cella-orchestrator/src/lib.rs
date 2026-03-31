//! Container lifecycle orchestration for cella.
//!
//! This crate extracts the shared container management logic so that both
//! the CLI (`cella`) and daemon (`cella-daemon`) can call the same Rust
//! functions instead of the daemon shelling out to CLI subprocesses.

pub mod branch;
pub mod config;
pub mod config_map;
pub mod container_setup;
pub mod docker_helpers;
pub mod error;
pub mod host_requirements;
pub mod image;
pub mod lifecycle;
pub mod progress;
pub mod prune;
pub mod result;
pub mod up;

pub use config::{BranchConfig, ImageStrategy, PruneConfig, UpConfig};
pub use error::OrchestratorError;
pub use progress::{PhaseChildHandle, PhaseHandle, ProgressEvent, ProgressSender, StepHandle};
pub use result::{
    BranchResult, ExecResult, PruneResult, PrunedEntry, UpOutcome, UpResult, WorktreeStatus,
};
