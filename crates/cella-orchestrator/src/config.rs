//! Input configuration types for orchestrator operations.

use std::collections::HashMap;
use std::path::PathBuf;

use cella_backend::BuildSecret;
use cella_config::devcontainer::resolve::ResolvedConfig;
pub use cella_network::NetworkRulePolicy;

/// Build/backend tuning inputs resolved from the `up` CLI flags.
///
/// Split into toolchain inputs (`docker_path`, `use_buildkit`) that apply to
/// every build site, and cache-I/O inputs (`cli_cache_from`, `cache_to`) that
/// apply to the base Dockerfile build only — the analog of the user's own
/// Dockerfile in the official single combined build.
#[derive(Clone, Copy)]
pub struct BuildTuning<'a> {
    /// Path to the `docker` CLI binary (`--docker-path`). `None` = `docker`.
    pub docker_path: Option<&'a str>,
    /// Whether `BuildKit`/buildx may be used (`--buildkit auto`). `false`
    /// forces the classic builder (`--buildkit never`).
    pub use_buildkit: bool,
    /// CLI `--cache-from` images, prepended before config `cacheFrom` on the
    /// base Dockerfile build (skipped entirely under `--no-cache`).
    pub cli_cache_from: &'a [String],
    /// CLI `--cache-to` cache export (`BuildKit` only; dropped otherwise).
    pub cache_to: Option<&'a str>,
}

impl Default for BuildTuning<'_> {
    /// `BuildKit` enabled (auto-probe), no toolchain overrides, no cache I/O —
    /// the standard build behavior for callers without `up` tuning flags.
    fn default() -> Self {
        Self {
            docker_path: None,
            use_buildkit: true,
            cli_cache_from: &[],
            cache_to: None,
        }
    }
}

impl BuildTuning<'_> {
    /// Toolchain-only view (docker binary + `BuildKit` decision) for build
    /// sites that must NOT inherit cache I/O (features layer, UID remap).
    #[must_use]
    pub const fn toolchain(self) -> Self {
        Self {
            docker_path: self.docker_path,
            use_buildkit: self.use_buildkit,
            cli_cache_from: &[],
            cache_to: None,
        }
    }
}

/// Options that shape the persisted `devcontainer.metadata` image/container
/// label.
///
/// Grouped so the label-shaping concern lives in one place and can grow
/// additively (e.g. a future `pickConfigProperties` whitelist for full
/// official parity) without widening `UpConfig`'s bool count.
#[derive(Clone, Copy, Default)]
pub struct MetadataOptions {
    /// `--omit-config-remote-env-from-metadata`: strip `remoteEnv` from the
    /// generated `devcontainer.metadata` label. Does NOT affect the runtime
    /// `dev.cella.remote_env` label used to re-inject env across restarts.
    pub omit_remote_env: bool,
}

/// Dotfiles installation inputs resolved from the `--dotfiles-*` CLI flags.
///
/// `repository` being `Some` is what arms the install (a `None` skips it
/// entirely). The repository value is expected to be already normalized by the
/// caller (owner/repo shorthand expanded to a full GitHub URL) — the installer
/// passes it to `git clone` verbatim. Grouped so the dotfiles concern threads
/// as one unit and can grow additively (e.g. a future `--dotfiles-branch`).
#[derive(Clone, Default)]
pub struct DotfilesConfig {
    /// `--dotfiles-repository`: clone source. `None` disables dotfiles install.
    pub repository: Option<String>,
    /// `--dotfiles-install-command`: explicit install script. `None` autodetects.
    pub install_command: Option<String>,
    /// `--dotfiles-target-path`: in-container clone target (default `~/dotfiles`).
    pub target_path: String,
}

/// Mount-related CLI flags for workspace configuration.
pub struct MountFlags<'a> {
    /// Additional mount points from CLI `--mount` flags.
    pub additional_cli_mounts: &'a [cella_backend::MountConfig],
    /// Workspace mount consistency mode (e.g. "cached", "delegated").
    pub workspace_mount_consistency: Option<&'a str>,
    /// Whether to mount the git root instead of the workspace folder.
    pub mount_workspace_git_root: bool,
    /// Whether to mount the git worktree common dir.
    pub mount_git_worktree_common_dir: bool,
}

/// Configuration for the full container-up pipeline.
pub struct UpConfig<'a> {
    /// Fully resolved devcontainer configuration.
    pub resolved: &'a ResolvedConfig,
    /// Resolved container name for this workspace.
    pub container_name: &'a str,
    /// Remote environment entries from config (`remoteEnv`). Used for lifecycle
    /// command env AND persisted in the `dev.cella.remote_env` metadata label.
    pub remote_env: &'a [String],
    /// Remote environment entries from the CLI `--remote-env` flag. Applied to
    /// lifecycle command env ONLY (config `remote_env` wins on key collision);
    /// runtime-only — must NOT enter labels, image layers, or `containerEnv`.
    pub cli_remote_env: &'a [String],
    /// Optional workspace folder override from config.
    pub workspace_folder_from_config: Option<&'a str>,
    /// Default workspace folder inside the container.
    pub default_workspace_folder: &'a str,
    /// Extra labels to merge into the container (e.g. worktree metadata).
    pub extra_labels: &'a HashMap<String, String>,
    /// CLI `--id-label` values (`key=value`). When non-empty these are set on a
    /// newly created container AND used (AND-matched) to find an existing one,
    /// replacing the default workspace-path lookup.
    pub id_labels: &'a [String],
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
    /// Mount-related CLI flags.
    pub mount_flags: MountFlags<'a>,
    /// Secrets injected into lifecycle commands as environment variables.
    /// Runtime-only — must NOT be stored in labels or image layers.
    pub lifecycle_secrets: &'a [String],
    /// Resolved `userEnvProbe` type (config value or CLI default).
    pub user_env_probe: cella_env::user_env_probe::UserEnvProbe,
    /// Gates which lifecycle phases run, and how, for this `up` (built from
    /// `--skip-post-create` / `--skip-non-blocking-commands` / `--prebuild` /
    /// `--skip-post-attach`). `Default` reproduces standard cella behavior.
    pub lifecycle_gate: cella_backend::LifecycleGate,
    /// `--expect-existing-container`: when `true`, fail (rather than create)
    /// if no container is found for this workspace. Gates before any build.
    pub expect_existing_container: bool,
    /// Build/backend tuning (`--docker-path`, `--buildkit`, `--cache-from`,
    /// `--cache-to`).
    pub build_tuning: BuildTuning<'a>,
    /// `--gpu-availability`: whether a config-requested GPU is granted.
    pub gpu_availability: cella_backend::GpuAvailability,
    /// `--update-remote-user-uid-default`: default for `updateRemoteUserUID`.
    pub update_remote_user_uid_default: cella_backend::UpdateRemoteUserUidDefault,
    /// Options shaping the persisted `devcontainer.metadata` label.
    pub metadata_options: MetadataOptions,
    /// Dotfiles install inputs (`--dotfiles-repository` / `-install-command` /
    /// `-target-path`). Installed in the post-create flow when armed and the
    /// lifecycle gate permits (after `postCreateCommand`, before `postStart`).
    pub dotfiles: DotfilesConfig,
    /// Lockfile policy for feature resolution (default: Update).
    pub lockfile_policy: cella_features::LockfilePolicy,
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
