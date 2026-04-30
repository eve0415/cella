//! Container lifecycle orchestration for cella.
//!
//! This crate extracts the shared container management logic so that both
//! the CLI (`cella`) and daemon (`cella-daemon`) can call the same Rust
//! functions instead of the daemon shelling out to CLI subprocesses.

pub mod branch;
pub use cella_compose::build_features as compose_build;
pub use cella_compose::combined_dockerfile_build as compose_features;
pub use cella_compose::mount_parity as compose_mounts;
pub use cella_compose::orchestrate as compose_up;
pub mod config;
pub use cella_backend::container_setup;
pub use cella_config::config_map;
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

/// Build a [`SubstitutionContext`] from a resolved config.
fn subst_ctx(
    resolved: &cella_config::devcontainer::resolve::ResolvedConfig,
) -> cella_config::devcontainer::subst::SubstitutionContext {
    cella_config::devcontainer::subst::SubstitutionContext::new(
        &resolved.workspace_root,
        resolved
            .config
            .get("workspaceFolder")
            .and_then(|v| v.as_str()),
        &resolved.devcontainer_id,
        std::env::vars().collect(),
    )
}

pub use config::{
    BranchConfig, HostRequirementPolicy, ImageStrategy, NetworkRulePolicy, PruneConfig, UpConfig,
};
pub use error::OrchestratorError;
pub use progress::{PhaseChildHandle, PhaseHandle, ProgressEvent, ProgressSender, StepHandle};
pub use result::{
    BranchResult, ExecResult, PruneResult, PrunedEntry, SshAgentProxyStatus, UpOutcome, UpResult,
    WorktreeStatus,
};
