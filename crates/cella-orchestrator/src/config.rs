//! Input configuration types for orchestrator operations.

use std::collections::HashMap;
use std::path::PathBuf;

use cella_backend::BuildSecret;
use cella_config::devcontainer::resolve::ResolvedConfig;
pub use cella_network::NetworkRulePolicy;

/// Configuration for the full container-up pipeline.
pub struct UpConfig<'a> {
    /// Fully resolved devcontainer configuration.
    pub resolved: &'a ResolvedConfig,
    /// Resolved container name for this workspace.
    pub container_name: &'a str,
    /// Remote environment entries from config.
    pub remote_env: &'a [String],
    /// Optional workspace folder override from config.
    pub workspace_folder_from_config: Option<&'a str>,
    /// Default workspace folder inside the container.
    pub default_workspace_folder: &'a str,
    /// Extra labels to merge into the container (e.g. worktree metadata).
    pub extra_labels: &'a HashMap<String, String>,
    /// How to handle the container image.
    pub image_strategy: ImageStrategy,
    /// Whether to remove an existing container before starting.
    pub remove_existing_container: bool,
    /// Skip SHA256 verification for managed agent downloads.
    pub skip_checksum: bool,
    /// How to treat unmet host requirements.
    pub host_requirement_policy: HostRequirementPolicy,
    /// Whether network rules are enforced.
    pub network_rule_policy: NetworkRulePolicy,
    /// Image pull policy (e.g. "always", "missing", "never").
    pub pull_policy: Option<&'a str>,
    /// `BuildKit` secrets to inject during image builds.
    pub build_secrets: Vec<BuildSecret>,
    /// Extra Docker networks to connect the container to after start,
    /// before lifecycle hooks run (e.g. parent compose network for worktrees).
    pub extra_networks: Vec<String>,
}

/// How the up pipeline should handle the container image.
#[derive(Clone, Copy, Default)]
pub enum ImageStrategy {
    /// Use cached image if available, build only if missing.
    #[default]
    Cached,
    /// Force rebuild the image using Docker cache.
    Rebuild,
    /// Force rebuild without Docker cache.
    RebuildNoCache,
}

/// How unmet host requirements should be handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRequirementPolicy {
    Warn,
    Error,
}

/// Configuration for creating a worktree-backed branch container.
pub struct BranchConfig {
    /// Git repository root path (host filesystem).
    pub repo_root: PathBuf,

    /// Branch name to create or check out.
    pub branch: String,

    /// Base ref for new branches (defaults to HEAD).
    pub base: Option<String>,

    /// Command to execute in the new container after creation.
    pub exec_cmd: Option<String>,
}

/// Configuration for pruning merged worktrees.
pub struct PruneConfig {
    /// Git repository root path (host filesystem).
    pub repo_root: PathBuf,

    /// Dry-run mode (report what would be pruned without doing it).
    pub dry_run: bool,
}
