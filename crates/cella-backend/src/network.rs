//! Types describing cella-managed Docker networks.

use std::collections::HashMap;

/// A cella-managed Docker network, as returned by
/// [`ContainerBackend::list_managed_networks`](crate::ContainerBackend::list_managed_networks).
///
/// "Managed" means the network carries the `dev.cella.managed=true`
/// label. cella creates both a shared `cella` network (cross-container
/// DNS hub) and per-workspace `cella-net-{hash}` networks; both are
/// reported here uniformly.
#[derive(Debug, Clone)]
pub struct ManagedNetwork {
    /// Network name (e.g. `cella` or `cella-net-abcdef123456`).
    pub name: String,
    /// Value of the `dev.cella.repo` label, if set. Only per-repo
    /// networks carry this.
    pub repo_path: Option<String>,
    /// Number of attached container endpoints. Includes stopped
    /// containers whose endpoints haven't been cleaned up.
    pub container_count: usize,
    /// Creation timestamp in RFC 3339 format, if available.
    pub created_at: Option<String>,
    /// Full label map. Callers that only need `dev.cella.repo` should
    /// use [`Self::repo_path`].
    pub labels: HashMap<String, String>,
}

/// Outcome of a single network-removal attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    /// The network was removed.
    Removed,
    /// The network had attached container endpoints; left in place.
    SkippedInUse,
    /// The network does not exist (either never existed or was removed
    /// by a concurrent caller).
    NotFound,
}
