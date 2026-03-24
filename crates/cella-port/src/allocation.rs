//! Port allocation table for multi-container port conflict resolution.
//!
//! Tracks which host ports are allocated to which containers and
//! resolves conflicts when multiple containers expose the same port.

use std::collections::HashMap;

use crate::CellaPortError;

/// A forwarded port mapping from container to host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedPort {
    /// Port inside the container.
    pub container_port: u16,
    /// Port on the host (may differ from `container_port` if remapped).
    pub host_port: u16,
    /// Container ID that owns this forwarding.
    pub container_id: String,
    /// Container name for display purposes.
    pub container_name: String,
}

/// Port allocation table managing host port assignments across containers.
#[derive(Debug, Default)]
pub struct PortAllocationTable {
    /// Map of `host_port` -> allocation.
    allocations: HashMap<u16, PortAllocation>,
    /// Default range for auto-allocation.
    range_start: u16,
    range_end: u16,
}

/// A single host port allocation.
#[derive(Debug, Clone)]
struct PortAllocation {
    host_port: u16,
    container_port: u16,
    container_id: String,
}

/// Scan a range of ports for the first available one in the allocation table.
fn scan_range(
    allocations: &mut HashMap<u16, PortAllocation>,
    range: impl Iterator<Item = u16>,
    container_port: u16,
    container_id: &str,
    is_available: &impl Fn(u16) -> bool,
) -> Option<u16> {
    for port in range {
        if let std::collections::hash_map::Entry::Vacant(e) = allocations.entry(port)
            && is_available(port)
        {
            e.insert(PortAllocation {
                host_port: port,
                container_port,
                container_id: container_id.to_string(),
            });
            return Some(port);
        }
    }
    None
}

impl PortAllocationTable {
    /// Create a new allocation table with default port range.
    pub fn new() -> Self {
        Self {
            allocations: HashMap::new(),
            range_start: 1024,
            range_end: 65535,
        }
    }

    /// Create with a custom port range.
    pub fn with_range(range_start: u16, range_end: u16) -> Self {
        Self {
            allocations: HashMap::new(),
            range_start,
            range_end,
        }
    }

    /// Allocate a host port for a container port.
    ///
    /// Strategy:
    /// - First container gets the native port (3000 -> 3000).
    /// - If the native port is taken, increment until a free port is found.
    ///
    /// # Errors
    ///
    /// Returns `CellaPortError::PortInUse` if `require_local_port` is true and
    /// the exact port is unavailable.
    /// Returns `CellaPortError::NoAvailablePorts` if no port in range is free.
    pub fn allocate(
        &mut self,
        container_port: u16,
        container_id: &str,
        require_local_port: bool,
    ) -> Result<u16, CellaPortError> {
        self.allocate_with_check(container_port, container_id, require_local_port, |_| true)
    }

    /// Allocate a host port, checking both the internal table and an external
    /// availability predicate (e.g., OS port binding).
    ///
    /// The `is_available` predicate is called for each candidate port that is
    /// free in the internal table. If it returns `false`, the port is skipped.
    ///
    /// # Errors
    ///
    /// Same as [`allocate`].
    pub fn allocate_with_check(
        &mut self,
        container_port: u16,
        container_id: &str,
        require_local_port: bool,
        is_available: impl Fn(u16) -> bool,
    ) -> Result<u16, CellaPortError> {
        // Try the native port first
        if !self.allocations.contains_key(&container_port)
            && self.in_range(container_port)
            && is_available(container_port)
        {
            self.allocations.insert(
                container_port,
                PortAllocation {
                    host_port: container_port,
                    container_port,
                    container_id: container_id.to_string(),
                },
            );
            return Ok(container_port);
        }

        if require_local_port {
            return Err(CellaPortError::PortInUse(container_port));
        }

        // Sequential scan from container_port + 1, then wrap around from range_start
        let start = container_port.saturating_add(1).max(self.range_start);
        if let Some(port) = scan_range(
            &mut self.allocations,
            (start..=self.range_end).chain(self.range_start..container_port),
            container_port,
            container_id,
            &is_available,
        ) {
            return Ok(port);
        }

        Err(CellaPortError::NoAvailablePorts)
    }

    /// Release all ports allocated to a container.
    pub fn release_container(&mut self, container_id: &str) {
        self.allocations
            .retain(|_, alloc| alloc.container_id != container_id);
    }

    /// Release a single host port allocation.
    pub fn release_port(&mut self, host_port: u16) {
        self.allocations.remove(&host_port);
    }

    /// Get all forwarded ports for a container.
    pub fn container_ports(&self, container_id: &str) -> Vec<ForwardedPort> {
        self.allocations
            .values()
            .filter(|a| a.container_id == container_id)
            .map(|a| ForwardedPort {
                container_port: a.container_port,
                host_port: a.host_port,
                container_id: a.container_id.clone(),
                container_name: String::new(), // filled by caller
            })
            .collect()
    }

    /// Get all allocations across all containers.
    pub fn all_ports(&self) -> Vec<ForwardedPort> {
        self.allocations
            .values()
            .map(|a| ForwardedPort {
                container_port: a.container_port,
                host_port: a.host_port,
                container_id: a.container_id.clone(),
                container_name: String::new(),
            })
            .collect()
    }

    /// Check if a host port is already allocated.
    pub fn is_allocated(&self, host_port: u16) -> bool {
        self.allocations.contains_key(&host_port)
    }

    const fn in_range(&self, port: u16) -> bool {
        port >= self.range_start && port <= self.range_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_native_port() {
        let mut table = PortAllocationTable::new();
        let port = table.allocate(3000, "container-a", false).unwrap();
        assert_eq!(port, 3000);
    }

    #[test]
    fn allocate_remaps_on_conflict() {
        let mut table = PortAllocationTable::new();
        let p1 = table.allocate(3000, "container-a", false).unwrap();
        let p2 = table.allocate(3000, "container-b", false).unwrap();
        assert_eq!(p1, 3000);
        assert_eq!(p2, 3001);
    }

    #[test]
    fn require_local_port_fails_on_conflict() {
        let mut table = PortAllocationTable::new();
        table.allocate(3000, "container-a", false).unwrap();
        let result = table.allocate(3000, "container-b", true);
        assert!(matches!(result, Err(CellaPortError::PortInUse(3000))));
    }

    #[test]
    fn release_frees_ports() {
        let mut table = PortAllocationTable::new();
        table.allocate(3000, "container-a", false).unwrap();
        table.allocate(8080, "container-a", false).unwrap();
        table.release_container("container-a");
        assert!(!table.is_allocated(3000));
        assert!(!table.is_allocated(8080));
    }

    #[test]
    fn container_ports_returns_owned() {
        let mut table = PortAllocationTable::new();
        table.allocate(3000, "container-a", false).unwrap();
        table.allocate(8080, "container-b", false).unwrap();
        let ports = table.container_ports("container-a");
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, 3000);
    }

    #[test]
    fn multiple_ports_different_containers() {
        let mut table = PortAllocationTable::new();
        table.allocate(3000, "a", false).unwrap();
        table.allocate(3000, "b", false).unwrap();
        table.allocate(5432, "a", false).unwrap();
        table.allocate(5432, "b", false).unwrap();

        let all = table.all_ports();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn custom_range() {
        let mut table = PortAllocationTable::with_range(10000, 10010);
        // Port 3000 is outside range, so it won't be allocated directly
        let port = table.allocate(3000, "a", false).unwrap();
        assert_eq!(port, 10000);
    }

    #[test]
    fn exhausted_range() {
        let mut table = PortAllocationTable::with_range(10000, 10001);
        table.allocate(10000, "a", false).unwrap();
        table.allocate(10001, "b", false).unwrap();
        let result = table.allocate(10000, "c", false);
        assert!(matches!(result, Err(CellaPortError::NoAvailablePorts)));
    }

    #[test]
    fn release_port_frees_single_port() {
        let mut table = PortAllocationTable::new();
        table.allocate(3000, "a", false).unwrap();
        table.allocate(8080, "a", false).unwrap();
        table.release_port(3000);
        assert!(!table.is_allocated(3000));
        assert!(table.is_allocated(8080));
    }

    #[test]
    fn allocate_with_check_skips_unavailable_native() {
        let mut table = PortAllocationTable::new();
        let port = table
            .allocate_with_check(3000, "a", false, |p| p != 3000)
            .unwrap();
        assert_eq!(port, 3001);
    }

    #[test]
    fn allocate_with_check_require_local_fails_when_os_taken() {
        let mut table = PortAllocationTable::new();
        let result = table.allocate_with_check(3000, "a", true, |p| p != 3000);
        assert!(matches!(result, Err(CellaPortError::PortInUse(3000))));
    }

    #[test]
    fn allocate_with_check_skips_multiple_unavailable() {
        let mut table = PortAllocationTable::new();
        let port = table
            .allocate_with_check(3000, "a", false, |p| p > 3002)
            .unwrap();
        assert_eq!(port, 3003);
    }
}
