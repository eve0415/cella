//! Output types returned by orchestrator operations.

use std::path::PathBuf;

/// Result of the container-up pipeline.
pub struct UpResult {
    /// Docker container ID.
    pub container_id: String,

    /// Container name.
    pub container_name: String,

    /// Remote user inside the container.
    pub remote_user: String,

    /// Workspace folder path inside the container.
    pub workspace_folder: String,

    /// What happened during the up operation.
    pub outcome: UpOutcome,

    /// SSH-agent proxy status, when an SSH-agent forwarding decision
    /// was actually surfaced for this container. `None` means the
    /// proxy code path was not exercised (no host agent, user override,
    /// or non-colima runtime that uses direct mount).
    pub ssh_agent_proxy: Option<SshAgentProxyStatus>,
}

/// Outcome of the SSH-agent bridge resolution at `cella up`. Cella-cli
/// renders this as a one-line status under the container info.
#[derive(Debug, Clone)]
pub enum SshAgentProxyStatus {
    /// Daemon-managed bridge was registered. `host_endpoint` is the
    /// `host:port` the in-container agent will bridge to; `refcount`
    /// is the post-register count.
    Bridged {
        host_endpoint: String,
        refcount: usize,
    },
    /// Bridge was requested (colima with `SSH_AUTH_SOCK` set) but the
    /// daemon RPC failed; SSH forwarding was skipped. `reason` is a
    /// short human-readable explanation.
    Skipped { reason: String },
}

/// What the up pipeline did.
pub enum UpOutcome {
    /// Container was already running (ran postAttach only).
    Running,
    /// Stopped container was restarted.
    Started,
    /// New container was created from scratch.
    Created,
}

/// Result of creating a worktree-backed branch container.
pub struct BranchResult {
    /// Path to the git worktree on the host.
    pub worktree_path: PathBuf,

    /// Docker container ID.
    pub container_id: String,

    /// Container name.
    pub container_name: String,

    /// Remote user inside the container.
    pub remote_user: String,

    /// Workspace folder path inside the container.
    pub workspace_folder: String,
}

/// A worktree with its optional container status.
pub struct WorktreeStatus {
    /// Worktree directory path on the host.
    pub path: PathBuf,

    /// Branch name (`None` if detached HEAD).
    pub branch: Option<String>,

    /// Whether this is the main (non-linked) worktree.
    pub is_main: bool,

    /// Associated container name, if any.
    pub container_name: Option<String>,

    /// Container state (e.g. "running", "exited").
    pub container_state: Option<String>,
}

/// Result of a prune operation.
pub struct PruneResult {
    /// Branches that were successfully pruned.
    pub pruned: Vec<PrunedEntry>,

    /// Errors encountered during pruning.
    pub errors: Vec<String>,
}

/// A single pruned worktree.
pub struct PrunedEntry {
    /// Branch name that was pruned.
    pub branch: String,

    /// Whether a container was also removed.
    pub had_container: bool,
}

/// Result of executing a command in a container.
pub struct ExecResult {
    /// Process exit code.
    pub exit_code: i32,
}
