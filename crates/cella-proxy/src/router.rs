//! Route table for hostname-based request routing.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

/// Lookup key for a hostname route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteKey {
    pub project: String,
    pub branch: String,
    pub port: u16,
}

/// How to reach the backend container.
#[derive(Debug, Clone)]
pub enum ProxyMode {
    DirectIp(IpAddr),
    AgentTunnel(String),
}

/// Target backend for a matched route.
#[derive(Debug, Clone)]
pub struct BackendTarget {
    pub container_id: String,
    pub container_name: String,
    pub target_port: u16,
    pub mode: ProxyMode,
}

/// Thread-safe route table for hostname → backend lookups.
#[derive(Debug, Default)]
pub struct RouteTable {
    routes: HashMap<RouteKey, BackendTarget>,
    defaults: HashMap<(String, String), u16>,
    by_container: HashMap<String, HashSet<RouteKey>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a route. Overwrites any existing route for the same key.
    pub fn insert(&mut self, key: RouteKey, target: BackendTarget) {
        self.by_container
            .entry(target.container_id.clone())
            .or_default()
            .insert(key.clone());
        self.routes.insert(key, target);
    }

    /// Set the default port for a (project, branch) pair.
    pub fn set_default_port(&mut self, project: &str, branch: &str, port: u16) {
        self.defaults
            .insert((project.to_string(), branch.to_string()), port);
    }

    /// Look up a route by exact key.
    pub fn lookup(&self, project: &str, branch: &str, port: u16) -> Option<&BackendTarget> {
        let key = RouteKey {
            project: project.to_string(),
            branch: branch.to_string(),
            port,
        };
        self.routes.get(&key)
    }

    /// Look up the default port for a (project, branch), then resolve the route.
    pub fn lookup_default(&self, project: &str, branch: &str) -> Option<&BackendTarget> {
        let port = self
            .defaults
            .get(&(project.to_string(), branch.to_string()))?;
        self.lookup(project, branch, *port)
    }

    /// Remove a single route.
    pub fn remove(&mut self, key: &RouteKey) -> Option<BackendTarget> {
        let target = self.routes.remove(key)?;
        if let Some(keys) = self.by_container.get_mut(&target.container_id) {
            keys.remove(key);
            if keys.is_empty() {
                self.by_container.remove(&target.container_id);
            }
        }
        Some(target)
    }

    /// Remove all routes for a container. Returns the removed keys.
    pub fn remove_container(&mut self, container_id: &str) -> Vec<RouteKey> {
        let keys = self.by_container.remove(container_id).unwrap_or_default();
        let mut removed = Vec::with_capacity(keys.len());
        for key in keys {
            self.routes.remove(&key);
            // Clean up defaults that reference removed routes
            let default_key = (key.project.clone(), key.branch.clone());
            if self.defaults.get(&default_key) == Some(&key.port) {
                self.defaults.remove(&default_key);
            }
            removed.push(key);
        }
        removed
    }

    /// Update the proxy mode for all routes belonging to a container.
    pub fn update_container_mode(&mut self, container_id: &str, mode: &ProxyMode) {
        if let Some(keys) = self.by_container.get(container_id) {
            let keys: Vec<_> = keys.iter().cloned().collect();
            for key in keys {
                if let Some(target) = self.routes.get_mut(&key) {
                    target.mode = mode.clone();
                }
            }
        }
    }

    /// All registered routes (for error page listing).
    pub fn all_routes(&self) -> impl Iterator<Item = (&RouteKey, &BackendTarget)> {
        self.routes.iter()
    }

    /// Number of active routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn test_key(project: &str, branch: &str, port: u16) -> RouteKey {
        RouteKey {
            project: project.to_string(),
            branch: branch.to_string(),
            port,
        }
    }

    fn test_target(container_id: &str, port: u16) -> BackendTarget {
        BackendTarget {
            container_id: container_id.to_string(),
            container_name: format!("cella-{container_id}"),
            target_port: port,
            mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::new(172, 20, 0, 2))),
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut rt = RouteTable::new();
        let key = test_key("myapp", "main", 3000);
        rt.insert(key, test_target("c1", 3000));

        let result = rt.lookup("myapp", "main", 3000);
        assert!(result.is_some());
        assert_eq!(result.unwrap().container_id, "c1");
    }

    #[test]
    fn lookup_miss() {
        let rt = RouteTable::new();
        assert!(rt.lookup("myapp", "main", 3000).is_none());
    }

    #[test]
    fn default_port_lookup() {
        let mut rt = RouteTable::new();
        let key = test_key("myapp", "main", 3000);
        rt.insert(key, test_target("c1", 3000));
        rt.set_default_port("myapp", "main", 3000);

        let result = rt.lookup_default("myapp", "main");
        assert!(result.is_some());
        assert_eq!(result.unwrap().target_port, 3000);
    }

    #[test]
    fn default_port_no_default_set() {
        let mut rt = RouteTable::new();
        let key = test_key("myapp", "main", 3000);
        rt.insert(key, test_target("c1", 3000));

        assert!(rt.lookup_default("myapp", "main").is_none());
    }

    #[test]
    fn remove_single_route() {
        let mut rt = RouteTable::new();
        let key = test_key("myapp", "main", 3000);
        rt.insert(key.clone(), test_target("c1", 3000));

        let removed = rt.remove(&key);
        assert!(removed.is_some());
        assert!(rt.lookup("myapp", "main", 3000).is_none());
        assert!(rt.is_empty());
    }

    #[test]
    fn remove_nonexistent() {
        let mut rt = RouteTable::new();
        let key = test_key("myapp", "main", 3000);
        assert!(rt.remove(&key).is_none());
    }

    #[test]
    fn remove_container_clears_all_routes() {
        let mut rt = RouteTable::new();
        rt.insert(test_key("myapp", "main", 3000), test_target("c1", 3000));
        rt.insert(test_key("myapp", "main", 8080), test_target("c1", 8080));
        rt.set_default_port("myapp", "main", 3000);

        let removed = rt.remove_container("c1");
        assert_eq!(removed.len(), 2);
        assert!(rt.is_empty());
        assert!(rt.lookup_default("myapp", "main").is_none());
    }

    #[test]
    fn remove_container_unknown_is_empty() {
        let mut rt = RouteTable::new();
        let removed = rt.remove_container("nonexistent");
        assert!(removed.is_empty());
    }

    #[test]
    fn remove_container_preserves_other_containers() {
        let mut rt = RouteTable::new();
        rt.insert(test_key("myapp", "main", 3000), test_target("c1", 3000));
        rt.insert(
            test_key("myapp", "feature-auth", 3000),
            test_target("c2", 3000),
        );

        rt.remove_container("c1");
        assert_eq!(rt.len(), 1);
        assert!(rt.lookup("myapp", "feature-auth", 3000).is_some());
    }

    #[test]
    fn update_container_mode() {
        let mut rt = RouteTable::new();
        rt.insert(test_key("myapp", "main", 3000), test_target("c1", 3000));
        rt.insert(test_key("myapp", "main", 8080), test_target("c1", 8080));

        rt.update_container_mode("c1", &ProxyMode::AgentTunnel("cella-c1".to_string()));

        let target = rt.lookup("myapp", "main", 3000).unwrap();
        assert!(matches!(target.mode, ProxyMode::AgentTunnel(ref n) if n == "cella-c1"));
        let target = rt.lookup("myapp", "main", 8080).unwrap();
        assert!(matches!(target.mode, ProxyMode::AgentTunnel(ref n) if n == "cella-c1"));
    }

    #[test]
    fn overwrite_existing_route() {
        let mut rt = RouteTable::new();
        let key = test_key("myapp", "main", 3000);
        rt.insert(key.clone(), test_target("c1", 3000));
        rt.insert(key, test_target("c2", 3000));

        let result = rt.lookup("myapp", "main", 3000).unwrap();
        assert_eq!(result.container_id, "c2");
        assert_eq!(rt.len(), 1);
    }

    #[test]
    fn len_and_is_empty() {
        let mut rt = RouteTable::new();
        assert!(rt.is_empty());
        assert_eq!(rt.len(), 0);

        rt.insert(test_key("myapp", "main", 3000), test_target("c1", 3000));
        assert!(!rt.is_empty());
        assert_eq!(rt.len(), 1);
    }

    #[test]
    fn all_routes_iterates() {
        let mut rt = RouteTable::new();
        rt.insert(test_key("myapp", "main", 3000), test_target("c1", 3000));
        rt.insert(test_key("myapp", "main", 8080), test_target("c1", 8080));

        assert_eq!(rt.all_routes().count(), 2);
    }

    #[test]
    fn multiple_containers_different_branches() {
        let mut rt = RouteTable::new();
        rt.insert(test_key("myapp", "main", 3000), test_target("c1", 3000));
        rt.insert(
            test_key("myapp", "feature-auth", 3000),
            test_target("c2", 3000),
        );
        rt.set_default_port("myapp", "main", 3000);
        rt.set_default_port("myapp", "feature-auth", 3000);

        assert_eq!(
            rt.lookup_default("myapp", "main").unwrap().container_id,
            "c1"
        );
        assert_eq!(
            rt.lookup_default("myapp", "feature-auth")
                .unwrap()
                .container_id,
            "c2"
        );
    }
}
