//! Container lifecycle orchestration for cella.
//!
//! This crate extracts the shared container management logic so that both
//! the CLI (`cella`) and daemon (`cella-daemon`) can call the same Rust
//! functions instead of the daemon shelling out to CLI subprocesses.

pub mod branch;
pub mod compose_build;
pub mod compose_features;
pub mod compose_mounts;
pub mod compose_up;
pub mod config;
pub mod config_map;
pub use cella_backend::container_setup;
pub mod daemon_registration;
pub mod docker_helpers;
pub mod env_cache;
pub mod error;
pub mod host_requirements;
pub mod image;
pub use cella_backend::lifecycle;
pub use cella_backend::progress;
pub mod prune;
pub mod result;
pub mod shell_detect;
pub use cella_backend::uid_image;
pub use cella_daemon_client::ssh_proxy as ssh_proxy_client;
pub use cella_tool_install as tool_install;
pub mod up;

pub use config::{
    BranchConfig, HostRequirementPolicy, ImageStrategy, NetworkRulePolicy, PruneConfig, UpConfig,
};
pub use error::OrchestratorError;
pub use progress::{PhaseChildHandle, PhaseHandle, ProgressEvent, ProgressSender, StepHandle};
pub use result::{
    BranchResult, ExecResult, PruneResult, PrunedEntry, SshAgentProxyStatus, UpOutcome, UpResult,
    WorktreeStatus,
};
