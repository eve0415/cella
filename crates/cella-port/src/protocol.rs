//! Control socket protocol types shared between cella-agent and cella-daemon.
//!
//! Messages are newline-delimited JSON over a Unix socket.

use serde::{Deserialize, Serialize};

/// Current protocol version for the agent↔daemon handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Sent by the agent as the first message after connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    pub protocol_version: u32,
    pub agent_version: String,
    /// Container name for routing (agent self-identifies).
    pub container_name: String,
    /// Auth token for validating the connection.
    pub auth_token: String,
}

/// Sent by the daemon in response to `AgentHello`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonHello {
    pub protocol_version: u32,
    pub daemon_version: String,
    /// If set, the daemon is rejecting the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Messages sent from the in-container agent to the host daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// A new port listener was detected.
    PortOpen {
        port: u16,
        protocol: PortProtocol,
        /// Process name (from /proc/<pid>/cmdline), if readable.
        process: Option<String>,
        /// Whether the listener binds localhost only (vs 0.0.0.0).
        bind: BindAddress,
        /// Port of the agent-side localhost proxy. When set, the daemon
        /// should connect to `container_ip:proxy_port` instead of
        /// `container_ip:port`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        proxy_port: Option<u16>,
    },
    /// A previously detected port listener has closed.
    PortClosed { port: u16, protocol: PortProtocol },
    /// Request to open a URL in the host browser.
    BrowserOpen { url: String },
    /// Git credential request forwarded from the container.
    CredentialRequest {
        /// Unique request ID for correlating responses.
        id: String,
        operation: String,
        fields: std::collections::HashMap<String, String>,
    },
    /// Periodic health heartbeat.
    Health {
        uptime_secs: u64,
        ports_detected: usize,
    },
}

// ---------------------------------------------------------------------------
// Management protocol (CLI ↔ daemon via ~/.cella/daemon.sock)
// ---------------------------------------------------------------------------

/// Requests from CLI tools to the daemon management socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagementRequest {
    /// Register a new container for port management.
    RegisterContainer {
        container_id: String,
        container_name: String,
        container_ip: Option<String>,
        ports_attributes: Vec<PortAttributes>,
        other_ports_attributes: Option<PortAttributes>,
        /// Ports from `forwardPorts` in devcontainer.json (pre-allocate on registration).
        #[serde(default)]
        forward_ports: Vec<u16>,
    },
    /// Deregister a container (stop proxies, release ports).
    DeregisterContainer { container_name: String },
    /// Query all forwarded ports across containers.
    QueryPorts,
    /// Query daemon status.
    QueryStatus,
    /// Health check.
    Ping,
    /// Request graceful shutdown of the daemon.
    Shutdown,
}

/// Responses from the daemon management socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagementResponse {
    /// Container successfully registered.
    ContainerRegistered { container_name: String },
    /// Container deregistered.
    ContainerDeregistered {
        container_name: String,
        ports_released: usize,
    },
    /// Forwarded port listing.
    Ports { ports: Vec<ForwardedPortDetail> },
    /// Daemon status.
    Status {
        pid: u32,
        uptime_secs: u64,
        container_count: usize,
        containers: Vec<ContainerSummary>,
        is_orbstack: bool,
        #[serde(default)]
        daemon_version: String,
        #[serde(default)]
        daemon_started_at: u64,
        /// TCP control port for agent connections.
        #[serde(default)]
        control_port: u16,
        /// Auth token for agent connections.
        #[serde(default)]
        control_token: String,
    },
    /// Daemon is shutting down.
    ShuttingDown { pid: u32 },
    /// Pong response.
    Pong,
    /// Error response.
    Error { message: String },
}

/// Detail about a single forwarded port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardedPortDetail {
    pub container_name: String,
    pub container_port: u16,
    pub host_port: u16,
    pub protocol: PortProtocol,
    pub process: Option<String>,
    pub url: String,
}

/// Summary of a registered container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSummary {
    pub container_name: String,
    pub container_id: String,
    pub forwarded_port_count: usize,
    pub agent_connected: bool,
    #[serde(default)]
    pub last_seen_secs: u64,
}

/// Messages sent from the host daemon to the in-container agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// Acknowledgment of a received message.
    Ack { id: Option<String> },
    /// Response to a credential request.
    CredentialResponse {
        id: String,
        fields: std::collections::HashMap<String, String>,
    },
    /// Configuration update from the daemon.
    Config {
        poll_interval_ms: u64,
        proxy_localhost: bool,
    },
    /// Port mapping notification: tells the agent which host port was allocated.
    PortMapping { container_port: u16, host_port: u16 },
}

/// Transport protocol for a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

impl std::fmt::Display for PortProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => f.write_str("tcp"),
            Self::Udp => f.write_str("udp"),
        }
    }
}

/// Whether a listener binds to localhost only or all interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindAddress {
    /// Bound to 127.0.0.1 / `::1` only.
    Localhost,
    /// Bound to 0.0.0.0 / :: (all interfaces).
    All,
}

/// Behavior when a port is auto-detected.
///
/// Matches the devcontainer spec `onAutoForward` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnAutoForward {
    /// Show a notification (default).
    #[default]
    Notify,
    /// Open in the default browser.
    OpenBrowser,
    /// Open in browser once (first detection only).
    OpenBrowserOnce,
    /// Open in a preview panel (treated as openBrowser in CLI context).
    OpenPreview,
    /// Forward silently without notification.
    Silent,
    /// Do not forward this port.
    Ignore,
}

/// Parsed per-port attributes from devcontainer.json `portsAttributes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortAttributes {
    /// Port number or pattern this applies to.
    pub port: PortPattern,
    /// What to do when this port is auto-detected.
    pub on_auto_forward: OnAutoForward,
    /// Display label for this port.
    pub label: Option<String>,
    /// Protocol hint for URL generation.
    pub protocol: Option<String>,
    /// Whether the exact host port is required (fail if unavailable).
    pub require_local_port: bool,
    /// Whether to attempt elevated access for privileged ports.
    pub elevate_if_needed: bool,
}

impl Default for PortAttributes {
    fn default() -> Self {
        Self {
            port: PortPattern::Single(0),
            on_auto_forward: OnAutoForward::default(),
            label: None,
            protocol: None,
            require_local_port: false,
            elevate_if_needed: false,
        }
    }
}

/// A port pattern for matching detected ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortPattern {
    /// Exact port number.
    Single(u16),
    /// Range of ports (inclusive).
    Range(u16, u16),
}

impl PortPattern {
    /// Check if a port number matches this pattern.
    pub const fn matches(&self, port: u16) -> bool {
        match self {
            Self::Single(p) => *p == port,
            Self::Range(lo, hi) => port >= *lo && port <= *hi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_agent_port_open() {
        let msg = AgentMessage::PortOpen {
            port: 3000,
            protocol: PortProtocol::Tcp,
            process: Some("node".to_string()),
            bind: BindAddress::Localhost,
            proxy_port: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"port_open\""));
        assert!(json.contains("\"port\":3000"));
        assert!(json.contains("\"process\":\"node\""));
        // proxy_port=None should be omitted
        assert!(!json.contains("proxy_port"));
    }

    #[test]
    fn serialize_agent_port_open_with_proxy_port() {
        let msg = AgentMessage::PortOpen {
            port: 3000,
            protocol: PortProtocol::Tcp,
            process: None,
            bind: BindAddress::Localhost,
            proxy_port: Some(50123),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"proxy_port\":50123"));
    }

    #[test]
    fn deserialize_port_open_without_proxy_port() {
        // Backward compatibility: older agents don't send proxy_port
        let json = r#"{"type":"port_open","port":3000,"protocol":"tcp","process":null,"bind":"localhost"}"#;
        let msg: AgentMessage = serde_json::from_str(json).unwrap();
        match msg {
            AgentMessage::PortOpen { proxy_port, .. } => assert_eq!(proxy_port, None),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn deserialize_agent_port_closed() {
        let json = r#"{"type":"port_closed","port":3000,"protocol":"tcp"}"#;
        let msg: AgentMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            AgentMessage::PortClosed {
                port: 3000,
                protocol: PortProtocol::Tcp
            }
        ));
    }

    #[test]
    fn serialize_daemon_ack() {
        let msg = DaemonMessage::Ack {
            id: Some("req-1".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ack\""));
    }

    #[test]
    fn roundtrip_browser_open() {
        let msg = AgentMessage::BrowserOpen {
            url: "https://github.com/login".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::BrowserOpen { url } if url == "https://github.com/login")
        );
    }

    #[test]
    fn roundtrip_credential_request() {
        let mut fields = std::collections::HashMap::new();
        fields.insert("protocol".to_string(), "https".to_string());
        fields.insert("host".to_string(), "github.com".to_string());

        let msg = AgentMessage::CredentialRequest {
            id: "cred-1".to_string(),
            operation: "get".to_string(),
            fields,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, AgentMessage::CredentialRequest { id, .. } if id == "cred-1"));
    }

    #[test]
    fn roundtrip_port_mapping() {
        let msg = DaemonMessage::PortMapping {
            container_port: 3000,
            host_port: 3001,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"port_mapping\""));
        assert!(json.contains("\"container_port\":3000"));
        assert!(json.contains("\"host_port\":3001"));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::PortMapping {
                container_port: 3000,
                host_port: 3001
            }
        ));
    }

    #[test]
    fn on_auto_forward_default_is_notify() {
        assert_eq!(OnAutoForward::default(), OnAutoForward::Notify);
    }

    #[test]
    fn port_pattern_single_match() {
        let p = PortPattern::Single(3000);
        assert!(p.matches(3000));
        assert!(!p.matches(3001));
    }

    #[test]
    fn port_pattern_range_match() {
        let p = PortPattern::Range(3000, 3010);
        assert!(p.matches(3000));
        assert!(p.matches(3005));
        assert!(p.matches(3010));
        assert!(!p.matches(2999));
        assert!(!p.matches(3011));
    }

    #[test]
    fn port_protocol_display() {
        assert_eq!(PortProtocol::Tcp.to_string(), "tcp");
        assert_eq!(PortProtocol::Udp.to_string(), "udp");
    }

    #[test]
    fn serialize_management_register() {
        let req = ManagementRequest::RegisterContainer {
            container_id: "abc123".to_string(),
            container_name: "cella-myapp-main".to_string(),
            container_ip: Some("172.20.0.5".to_string()),
            ports_attributes: vec![],
            other_ports_attributes: None,
            forward_ports: vec![],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"register_container\""));
        assert!(json.contains("\"container_ip\":\"172.20.0.5\""));
    }

    #[test]
    fn serialize_management_deregister() {
        let req = ManagementRequest::DeregisterContainer {
            container_name: "cella-myapp-main".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"deregister_container\""));
    }

    #[test]
    fn serialize_management_query_ports() {
        let req = ManagementRequest::QueryPorts;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"query_ports\""));
    }

    #[test]
    fn serialize_management_ping() {
        let req = ManagementRequest::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"ping\""));
    }

    #[test]
    fn roundtrip_management_response_pong() {
        let resp = ManagementResponse::Pong;
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, ManagementResponse::Pong));
    }

    #[test]
    fn roundtrip_management_response_ports() {
        let resp = ManagementResponse::Ports {
            ports: vec![ForwardedPortDetail {
                container_name: "test".to_string(),
                container_port: 3000,
                host_port: 3000,
                protocol: PortProtocol::Tcp,
                process: Some("node".to_string()),
                url: "localhost:3000".to_string(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, ManagementResponse::Ports { ports } if ports.len() == 1));
    }

    #[test]
    fn roundtrip_management_response_status() {
        let resp = ManagementResponse::Status {
            pid: 1234,
            uptime_secs: 60,
            container_count: 2,
            containers: vec![ContainerSummary {
                container_name: "test".to_string(),
                container_id: "abc".to_string(),
                forwarded_port_count: 1,
                agent_connected: true,
                last_seen_secs: 1000,
            }],
            is_orbstack: false,
            daemon_version: "0.1.0".to_string(),
            daemon_started_at: 1_700_000_000,
            control_port: 54321,
            control_token: "abc123".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementResponse::Status { pid: 1234, .. }
        ));
    }

    #[test]
    fn status_backward_compat_missing_version_fields() {
        // Old daemons won't have daemon_version/daemon_started_at
        let json = r#"{"type":"status","pid":1,"uptime_secs":0,"container_count":0,"containers":[],"is_orbstack":false}"#;
        let decoded: ManagementResponse = serde_json::from_str(json).unwrap();
        if let ManagementResponse::Status {
            daemon_version,
            daemon_started_at,
            ..
        } = decoded
        {
            assert!(daemon_version.is_empty());
            assert_eq!(daemon_started_at, 0);
        } else {
            panic!("Expected Status");
        }
    }

    #[test]
    fn roundtrip_agent_hello() {
        let hello = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            container_name: "test-container".to_string(),
            auth_token: "token123".to_string(),
        };
        let json = serde_json::to_string(&hello).unwrap();
        let decoded: AgentHello = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.agent_version, "0.1.0");
    }

    #[test]
    fn roundtrip_daemon_hello() {
        let hello = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".to_string(),
            error: None,
        };
        // Verify new AgentHello fields roundtrip
        let _hello2 = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            container_name: "test".to_string(),
            auth_token: "tok".to_string(),
        };
        let json = serde_json::to_string(&hello).unwrap();
        let decoded: DaemonHello = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert!(decoded.error.is_none());
    }

    #[test]
    fn roundtrip_daemon_hello_with_error() {
        let hello = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".to_string(),
            error: Some("version mismatch".to_string()),
        };
        let json = serde_json::to_string(&hello).unwrap();
        let decoded: DaemonHello = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.error.as_deref(), Some("version mismatch"));
    }

    #[test]
    fn roundtrip_shutdown() {
        let req = ManagementRequest::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"shutdown\""));
        let decoded: ManagementRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, ManagementRequest::Shutdown));
    }

    #[test]
    fn roundtrip_shutting_down() {
        let resp = ManagementResponse::ShuttingDown { pid: 42 };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementResponse::ShuttingDown { pid: 42 }
        ));
    }

    #[test]
    fn roundtrip_management_response_error() {
        let resp = ManagementResponse::Error {
            message: "something broke".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, ManagementResponse::Error { message } if message == "something broke")
        );
    }

    #[test]
    fn roundtrip_management_container_registered() {
        let resp = ManagementResponse::ContainerRegistered {
            container_name: "test".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, ManagementResponse::ContainerRegistered { container_name, .. } if container_name == "test")
        );
    }

    #[test]
    fn roundtrip_management_container_deregistered() {
        let resp = ManagementResponse::ContainerDeregistered {
            container_name: "test".to_string(),
            ports_released: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementResponse::ContainerDeregistered {
                ports_released: 3,
                ..
            }
        ));
    }
}
