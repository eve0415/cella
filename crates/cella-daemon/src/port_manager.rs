//! Port manager: tracks detected ports and manages host TCP proxies.

use std::collections::HashMap;

use cella_port::allocation::PortAllocationTable;
use cella_port::protocol::{OnAutoForward, PortAttributes, PortProtocol};
use tracing::{info, warn};

use crate::proxy::ProxyHandle;

/// Check if a host port is free at the OS level.
///
/// Uses a synchronous TCP bind probe. Fast (~1μs).
pub fn is_host_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Tracks detected ports and forwarded ports per container.
pub struct PortManager {
    /// Active containers and their detected ports.
    containers: HashMap<String, ContainerPorts>,
    /// Global port allocation table.
    allocation: PortAllocationTable,
    /// Whether we're running on `OrbStack` (skips TCP proxies).
    is_orbstack: bool,
    /// Active TCP proxy handles.
    proxy_handles: HashMap<u16, ProxyHandle>,
    /// Optional OS-level port availability check.
    port_checker: Option<Box<dyn Fn(u16) -> bool + Send + Sync>>,
}

/// Per-container port state.
struct ContainerPorts {
    container_name: String,
    container_ip: Option<String>,
    detected_ports: Vec<DetectedPort>,
    ports_attributes: Vec<PortAttributes>,
    other_ports_attributes: Option<PortAttributes>,
}

/// A port detected by the in-container agent.
#[derive(Debug, Clone)]
struct DetectedPort {
    port: u16,
    protocol: PortProtocol,
    process: Option<String>,
    host_port: Option<u16>,
}

impl PortManager {
    /// Create a new port manager.
    pub fn new(is_orbstack: bool) -> Self {
        Self {
            containers: HashMap::new(),
            allocation: PortAllocationTable::new(),
            is_orbstack,
            proxy_handles: HashMap::new(),
            port_checker: None,
        }
    }

    /// Set a custom port availability checker.
    ///
    /// The checker is called during allocation to verify the host port is
    /// actually free at the OS level.
    #[must_use]
    pub fn with_port_checker(
        mut self,
        checker: impl Fn(u16) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.port_checker = Some(Box::new(checker));
        self
    }

    /// Register a container for port management.
    pub fn register_container(
        &mut self,
        container_id: &str,
        container_name: &str,
        container_ip: Option<String>,
        ports_attributes: Vec<PortAttributes>,
        other_ports_attributes: Option<PortAttributes>,
    ) {
        self.containers.insert(
            container_id.to_string(),
            ContainerPorts {
                container_name: container_name.to_string(),
                container_ip,
                detected_ports: Vec::new(),
                ports_attributes,
                other_ports_attributes,
            },
        );
    }

    /// Get the container's IP address.
    pub fn container_ip(&self, container_id: &str) -> Option<&str> {
        self.containers
            .get(container_id)
            .and_then(|c| c.container_ip.as_deref())
    }

    /// Handle a port open event from an agent.
    ///
    /// Returns the allocated host port if successful.
    pub fn handle_port_open(
        &mut self,
        container_id: &str,
        port: u16,
        protocol: PortProtocol,
        process: Option<String>,
    ) -> Option<u16> {
        // Duplicate guard: if this container+port is already detected, return
        // the existing mapping. Handles agent reconnections re-reporting ports.
        if let Some(container) = self.containers.get(container_id)
            && let Some(existing) = container.detected_ports.iter().find(|d| d.port == port)
        {
            return existing.host_port;
        }

        // Extract all needed info from attrs before mutating self
        let (on_auto_forward, require_local, label, proto_hint) = {
            let attrs = self.find_port_attributes(container_id, port);
            (
                attrs.map_or(OnAutoForward::Notify, |a| a.on_auto_forward),
                attrs.is_some_and(|a| a.require_local_port),
                attrs.and_then(|a| a.label.clone()),
                attrs
                    .and_then(|a| a.protocol.clone())
                    .unwrap_or_else(|| "http".to_string()),
            )
        };

        if on_auto_forward == OnAutoForward::Ignore {
            info!("Port {port} ignored (onAutoForward: ignore)");
            return None;
        }

        // Allocate host port, checking OS availability if a checker is configured
        let host_port = match &self.port_checker {
            Some(checker) => self
                .allocation
                .allocate_with_check(port, container_id, require_local, checker.as_ref()),
            None => self.allocation.allocate(port, container_id, require_local),
        };
        let host_port = match host_port {
            Ok(hp) => hp,
            Err(e) => {
                warn!("Failed to allocate host port for {port}: {e}");
                return None;
            }
        };

        let process_str = process.as_deref().unwrap_or("unknown");
        let display_label = label.as_deref().unwrap_or("");
        if host_port == port {
            info!("Forwarding port {port} ({process_str}) {display_label}-> localhost:{host_port}");
        } else {
            info!(
                "Forwarding port {port} ({process_str}) {display_label}-> localhost:{host_port} (remapped)"
            );
        }

        let detected = DetectedPort {
            port,
            protocol,
            process,
            host_port: Some(host_port),
        };

        if let Some(container) = self.containers.get_mut(container_id) {
            container.detected_ports.push(detected);
        }

        match on_auto_forward {
            OnAutoForward::OpenBrowser | OnAutoForward::OpenBrowserOnce => {
                let url = format!("{proto_hint}://localhost:{host_port}");
                info!("Auto-opening browser: {url}");
            }
            OnAutoForward::Notify | OnAutoForward::Silent | OnAutoForward::OpenPreview
            | OnAutoForward::Ignore => {}
        }

        Some(host_port)
    }

    /// Handle a port closed event from an agent.
    ///
    /// Returns the host port that was released, if any.
    pub fn handle_port_closed(
        &mut self,
        container_id: &str,
        port: u16,
        _protocol: PortProtocol,
    ) -> Option<u16> {
        let host_port = self.containers.get(container_id).and_then(|c| {
            c.detected_ports
                .iter()
                .find(|p| p.port == port)
                .and_then(|p| p.host_port)
        });

        if let Some(container) = self.containers.get_mut(container_id) {
            container.detected_ports.retain(|p| p.port != port);
        }

        if let Some(hp) = host_port {
            self.allocation.release_port(hp);
            if let Some(handle) = self.proxy_handles.remove(&hp) {
                handle.abort();
            }
        }

        host_port
    }

    /// Unregister a container and release all its ports.
    pub fn unregister_container(&mut self, container_id: &str) {
        self.containers.remove(container_id);
        self.allocation.release_container(container_id);
    }

    /// Get all forwarded ports across all containers.
    pub fn all_forwarded_ports(&self) -> Vec<ForwardedPortInfo> {
        let mut result = Vec::new();
        for (container_id, container) in &self.containers {
            for detected in &container.detected_ports {
                if let Some(host_port) = detected.host_port {
                    result.push(ForwardedPortInfo {
                        container_id: container_id.clone(),
                        container_name: container.container_name.clone(),
                        container_port: detected.port,
                        host_port,
                        protocol: detected.protocol,
                        process: detected.process.clone(),
                        is_orbstack: self.is_orbstack,
                    });
                }
            }
        }
        result
    }

    /// Find matching port attributes for a port.
    fn find_port_attributes(&self, container_id: &str, port: u16) -> Option<&PortAttributes> {
        let container = self.containers.get(container_id)?;

        // Check specific port attributes first
        for attrs in &container.ports_attributes {
            if attrs.port.matches(port) {
                return Some(attrs);
            }
        }

        // Fall back to otherPortsAttributes
        container.other_ports_attributes.as_ref()
    }
}

/// Information about a forwarded port for display.
#[derive(Debug, Clone)]
pub struct ForwardedPortInfo {
    pub container_id: String,
    pub container_name: String,
    pub container_port: u16,
    pub host_port: u16,
    pub protocol: PortProtocol,
    pub process: Option<String>,
    pub is_orbstack: bool,
}

impl ForwardedPortInfo {
    /// Get the localhost access URL for this port.
    pub fn url(&self) -> String {
        format!("localhost:{}", self.host_port)
    }

    /// Get the OrbStack-specific URL (container.orb.local), if on OrbStack.
    pub fn orb_url(&self) -> Option<String> {
        if self.is_orbstack {
            Some(format!(
                "{}.orb.local:{}",
                self.container_name, self.container_port
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use cella_port::protocol::PortPattern;

    use super::*;

    #[test]
    fn register_and_detect_port() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "test-container", None, vec![], None);
        pm.handle_port_open("c1", 3000, PortProtocol::Tcp, Some("node".to_string()));

        let ports = pm.all_forwarded_ports();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, 3000);
        assert_eq!(ports[0].host_port, 3000);
    }

    #[test]
    fn port_remapping_on_conflict() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "container-a", None, vec![], None);
        pm.register_container("c2", "container-b", None, vec![], None);

        pm.handle_port_open("c1", 3000, PortProtocol::Tcp, None);
        pm.handle_port_open("c2", 3000, PortProtocol::Tcp, None);

        let ports = pm.all_forwarded_ports();
        assert_eq!(ports.len(), 2);

        let host_ports: Vec<u16> = ports.iter().map(|p| p.host_port).collect();
        assert!(host_ports.contains(&3000));
        assert!(host_ports.contains(&3001));
    }

    #[test]
    fn ignored_port_not_forwarded() {
        let mut pm = PortManager::new(false);
        let attrs = vec![PortAttributes {
            port: PortPattern::Single(9229),
            on_auto_forward: OnAutoForward::Ignore,
            ..PortAttributes::default()
        }];
        pm.register_container("c1", "test", None, attrs, None);
        pm.handle_port_open("c1", 9229, PortProtocol::Tcp, Some("node".to_string()));

        let ports = pm.all_forwarded_ports();
        assert!(ports.is_empty());
    }

    #[test]
    fn unregister_releases_ports() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "test", None, vec![], None);
        pm.handle_port_open("c1", 3000, PortProtocol::Tcp, None);

        pm.unregister_container("c1");

        let ports = pm.all_forwarded_ports();
        assert!(ports.is_empty());
    }

    #[test]
    fn register_stores_container_ip() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "test", Some("172.20.0.5".to_string()), vec![], None);
        assert_eq!(pm.container_ip("c1"), Some("172.20.0.5"));
        assert_eq!(pm.container_ip("c2"), None);
    }

    #[test]
    fn register_without_container_ip() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "test", None, vec![], None);
        assert_eq!(pm.container_ip("c1"), None);
    }

    #[test]
    fn orbstack_url_format() {
        let info = ForwardedPortInfo {
            container_id: "c1".to_string(),
            container_name: "cella-myapp-main".to_string(),
            container_port: 3000,
            host_port: 3000,
            protocol: PortProtocol::Tcp,
            process: Some("node".to_string()),
            is_orbstack: true,
        };
        // url() always returns localhost
        assert_eq!(info.url(), "localhost:3000");
        // orb_url() returns the OrbStack-specific URL
        assert_eq!(
            info.orb_url(),
            Some("cella-myapp-main.orb.local:3000".to_string())
        );
    }

    #[test]
    fn non_orbstack_has_no_orb_url() {
        let info = ForwardedPortInfo {
            container_id: "c1".to_string(),
            container_name: "test".to_string(),
            container_port: 3000,
            host_port: 3001,
            protocol: PortProtocol::Tcp,
            process: None,
            is_orbstack: false,
        };
        assert_eq!(info.orb_url(), None);
    }

    #[test]
    fn localhost_url_format() {
        let info = ForwardedPortInfo {
            container_id: "c1".to_string(),
            container_name: "test".to_string(),
            container_port: 3000,
            host_port: 3001,
            protocol: PortProtocol::Tcp,
            process: None,
            is_orbstack: false,
        };
        assert_eq!(info.url(), "localhost:3001");
    }

    #[test]
    fn duplicate_port_open_returns_existing() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "test", None, vec![], None);
        let hp1 = pm.handle_port_open("c1", 3000, PortProtocol::Tcp, Some("node".to_string()));
        let hp2 = pm.handle_port_open("c1", 3000, PortProtocol::Tcp, Some("node".to_string()));
        assert_eq!(hp1, Some(3000));
        assert_eq!(hp2, Some(3000));
        let ports = pm.all_forwarded_ports();
        assert_eq!(ports.len(), 1);
    }

    #[test]
    fn port_close_releases_allocation() {
        let mut pm = PortManager::new(false);
        pm.register_container("c1", "test", None, vec![], None);
        pm.handle_port_open("c1", 3000, PortProtocol::Tcp, None);
        pm.handle_port_closed("c1", 3000, PortProtocol::Tcp);
        // Re-opening should get the same port back, not 3001
        let hp = pm.handle_port_open("c1", 3000, PortProtocol::Tcp, None);
        assert_eq!(hp, Some(3000));
    }
}
