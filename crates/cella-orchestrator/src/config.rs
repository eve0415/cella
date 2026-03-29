//! Input configuration types for orchestrator operations.

use std::collections::HashMap;
use std::path::PathBuf;

/// Configuration for the full container-up pipeline.
pub struct UpConfig {
    /// Workspace root directory on the host.
    pub workspace_root: PathBuf,

    /// Explicit devcontainer.json file path (overrides auto-discovery).
    pub config_file: Option<PathBuf>,

    /// Docker host URL override (overrides `DOCKER_HOST`).
    pub docker_host: Option<String>,

    /// Extra labels to merge into the container (e.g. worktree metadata).
    pub extra_labels: HashMap<String, String>,

    /// How to handle the container image.
    pub image_strategy: ImageStrategy,

    /// Whether to remove an existing container before starting.
    pub remove_existing_container: bool,
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

/// Configuration for creating a worktree-backed branch container.
pub struct BranchConfig {
    /// Git repository root path (host filesystem).
    pub repo_root: PathBuf,

    /// Branch name to create or check out.
    pub branch: String,

    /// Base ref for new branches (defaults to HEAD).
    pub base: Option<String>,

    /// Docker host URL override.
    pub docker_host: Option<String>,

    /// Command to execute in the new container after creation.
    pub exec_cmd: Option<String>,
}

/// Configuration for pruning merged worktrees.
pub struct PruneConfig {
    /// Git repository root path (host filesystem).
    pub repo_root: PathBuf,

    /// Dry-run mode (report what would be pruned without doing it).
    pub dry_run: bool,

    /// Docker host URL override.
    pub docker_host: Option<String>,
}
