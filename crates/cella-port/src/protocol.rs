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
    /// Host-side workspace path (from container label `dev.cella.workspace_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Host-side parent repo root (set when this container is a worktree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_repo: Option<String>,
    /// Whether this container is a worktree-backed branch container.
    #[serde(default)]
    pub is_worktree: bool,
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

    // -- Worktree operations (in-container CLI → daemon) --------------------
    /// Request to create a worktree-backed branch and its container.
    BranchRequest {
        request_id: String,
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// Request to list worktree branches and their container status.
    ListRequest { request_id: String },
    /// Execute a command in another branch's container.
    ExecRequest {
        request_id: String,
        /// Branch name whose container to exec in.
        branch: String,
        /// Command to execute.
        command: Vec<String>,
    },
    /// Remove merged worktrees and their containers.
    PruneRequest {
        request_id: String,
        #[serde(default)]
        dry_run: bool,
    },
    /// Create a branch and run a background command in its container.
    TaskRunRequest {
        request_id: String,
        branch: String,
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// List active background tasks.
    TaskListRequest { request_id: String },
    /// Stream output from a background task.
    TaskLogsRequest { request_id: String, branch: String },
    /// Block until a background task completes.
    TaskWaitRequest { request_id: String, branch: String },
    /// Stop a running background task.
    TaskStopRequest { request_id: String, branch: String },
    /// Switch to another branch's container (run default shell).
    SwitchRequest { request_id: String, branch: String },
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
        /// The `shutdownAction` from devcontainer.json (`"none"` or `"stopContainer"`).
        #[serde(default)]
        shutdown_action: Option<String>,
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

    // -- Worktree operation responses (daemon → in-container agent) ---------
    /// Progress update for a long-running operation (branch creation, etc.).
    OperationProgress {
        request_id: String,
        step: String,
        message: String,
    },
    /// Streamed output (stdout/stderr) from a long-running operation.
    OperationOutput {
        request_id: String,
        stream: OutputStream,
        data: String,
    },
    /// Result of a branch creation request.
    BranchResult {
        request_id: String,
        #[serde(flatten)]
        result: WorktreeOperationResult,
    },
    /// Result of a worktree list request.
    ListResult {
        request_id: String,
        worktrees: Vec<WorktreeEntry>,
    },
    /// Result of an exec request (exit code after command completes).
    ExecResult { request_id: String, exit_code: i32 },
    /// Result of a prune request.
    PruneResult {
        request_id: String,
        pruned: Vec<String>,
        errors: Vec<String>,
    },
    /// A background task was started.
    TaskRunResult {
        request_id: String,
        task_id: String,
        container_name: String,
    },
    /// List of active background tasks.
    TaskListResult {
        request_id: String,
        tasks: Vec<TaskEntry>,
    },
    /// Background task output chunk (streaming).
    TaskLogsData {
        request_id: String,
        data: String,
        done: bool,
    },
    /// Background task completed.
    TaskWaitResult { request_id: String, exit_code: i32 },
    /// Background task stopped.
    TaskStopResult { request_id: String },
    /// Result of a switch (shell exec in target container).
    SwitchResult { request_id: String, exit_code: i32 },
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

// ---------------------------------------------------------------------------
// Worktree operation types
// ---------------------------------------------------------------------------

/// Which output stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Result of a worktree operation (success or error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorktreeOperationResult {
    Success {
        /// Container name of the newly created branch container.
        container_name: String,
        /// Host-side path to the worktree directory.
        worktree_path: String,
    },
    Error {
        message: String,
    },
}

/// A background task entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEntry {
    /// Task identifier (typically the branch name).
    pub task_id: String,
    /// Branch this task is running in.
    pub branch: String,
    /// Container running the task.
    pub container_name: String,
    /// Task status.
    pub status: TaskStatus,
    /// Command being run.
    pub command: Vec<String>,
    /// Seconds since the task started.
    pub elapsed_secs: u64,
}

/// Status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Done,
    Failed,
}

/// A worktree entry for list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeEntry {
    /// Branch name.
    pub branch: Option<String>,
    /// Host-side worktree path.
    pub worktree_path: String,
    /// Whether this is the main (non-linked) worktree.
    pub is_main: bool,
    /// Associated container name, if any.
    pub container_name: Option<String>,
    /// Container state (running, exited, etc.), if a container exists.
    pub container_state: Option<String>,
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
            shutdown_action: None,
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
            workspace_path: None,
            parent_repo: None,
            is_worktree: false,
        };
        let json = serde_json::to_string(&hello).unwrap();
        let decoded: DaemonHello = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert!(decoded.error.is_none());
        assert!(!decoded.is_worktree);
    }

    #[test]
    fn roundtrip_daemon_hello_with_workspace() {
        let hello = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".to_string(),
            error: None,
            workspace_path: Some("/home/user/project".to_string()),
            parent_repo: Some("/home/user/project".to_string()),
            is_worktree: true,
        };
        let json = serde_json::to_string(&hello).unwrap();
        let decoded: DaemonHello = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.workspace_path.as_deref(),
            Some("/home/user/project")
        );
        assert!(decoded.is_worktree);
    }

    #[test]
    fn daemon_hello_backward_compat_missing_workspace_fields() {
        // Old daemons won't send workspace metadata
        let json = r#"{"protocol_version":1,"daemon_version":"0.1.0"}"#;
        let decoded: DaemonHello = serde_json::from_str(json).unwrap();
        assert!(decoded.workspace_path.is_none());
        assert!(decoded.parent_repo.is_none());
        assert!(!decoded.is_worktree);
    }

    #[test]
    fn roundtrip_daemon_hello_with_error() {
        let hello = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".to_string(),
            error: Some("version mismatch".to_string()),
            workspace_path: None,
            parent_repo: None,
            is_worktree: false,
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

    // -- Worktree protocol tests -------------------------------------------

    #[test]
    fn roundtrip_branch_request() {
        let msg = AgentMessage::BranchRequest {
            request_id: "br-1".to_string(),
            branch: "feat/auth".to_string(),
            base: Some("main".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"branch_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::BranchRequest { request_id, branch, base }
                if request_id == "br-1" && branch == "feat/auth" && base.as_deref() == Some("main"))
        );
    }

    #[test]
    fn roundtrip_branch_request_no_base() {
        let msg = AgentMessage::BranchRequest {
            request_id: "br-2".to_string(),
            branch: "fix/bug".to_string(),
            base: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        // base=None should be omitted
        assert!(!json.contains("\"base\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::BranchRequest { base: None, .. }
        ));
    }

    #[test]
    fn roundtrip_list_request() {
        let msg = AgentMessage::ListRequest {
            request_id: "lr-1".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"list_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::ListRequest { request_id } if request_id == "lr-1")
        );
    }

    #[test]
    fn roundtrip_operation_progress() {
        let msg = DaemonMessage::OperationProgress {
            request_id: "br-1".to_string(),
            step: "Creating worktree".to_string(),
            message: "Creating worktree for 'feat/auth'...".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"operation_progress\""));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::OperationProgress { step, .. } if step == "Creating worktree"
        ));
    }

    #[test]
    fn roundtrip_operation_output() {
        let msg = DaemonMessage::OperationOutput {
            request_id: "br-1".to_string(),
            stream: OutputStream::Stdout,
            data: "Step 1/5: FROM ubuntu".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::OperationOutput {
                stream: OutputStream::Stdout,
                ..
            }
        ));
    }

    #[test]
    fn roundtrip_branch_result_success() {
        let msg = DaemonMessage::BranchResult {
            request_id: "br-1".to_string(),
            result: WorktreeOperationResult::Success {
                container_name: "cella-proj-feat-auth".to_string(),
                worktree_path: "/home/user/proj-worktrees/feat-auth".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::BranchResult {
                result: WorktreeOperationResult::Success { .. },
                ..
            }
        ));
    }

    #[test]
    fn roundtrip_branch_result_error() {
        let msg = DaemonMessage::BranchResult {
            request_id: "br-1".to_string(),
            result: WorktreeOperationResult::Error {
                message: "branch already checked out".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"status\":\"error\""));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::BranchResult {
                result: WorktreeOperationResult::Error { .. },
                ..
            }
        ));
    }

    #[test]
    fn roundtrip_list_result() {
        let msg = DaemonMessage::ListResult {
            request_id: "lr-1".to_string(),
            worktrees: vec![
                WorktreeEntry {
                    branch: Some("main".to_string()),
                    worktree_path: "/home/user/project".to_string(),
                    is_main: true,
                    container_name: Some("cella-proj-main".to_string()),
                    container_state: Some("running".to_string()),
                },
                WorktreeEntry {
                    branch: Some("feat/auth".to_string()),
                    worktree_path: "/home/user/project-worktrees/feat-auth".to_string(),
                    is_main: false,
                    container_name: None,
                    container_state: None,
                },
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, DaemonMessage::ListResult { worktrees, .. } if worktrees.len() == 2)
        );
    }

    // -- Phase 2 protocol tests --------------------------------------------

    #[test]
    fn roundtrip_exec_request() {
        let msg = AgentMessage::ExecRequest {
            request_id: "ex-1".to_string(),
            branch: "feat/auth".to_string(),
            command: vec!["cargo".to_string(), "test".to_string()],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"exec_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::ExecRequest { branch, .. } if branch == "feat/auth"
        ));
    }

    #[test]
    fn roundtrip_exec_result() {
        let msg = DaemonMessage::ExecResult {
            request_id: "ex-1".to_string(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::ExecResult { exit_code: 0, .. }
        ));
    }

    #[test]
    fn roundtrip_prune_request() {
        let msg = AgentMessage::PruneRequest {
            request_id: "pr-1".to_string(),
            dry_run: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::PruneRequest { dry_run: true, .. }
        ));
    }

    #[test]
    fn roundtrip_prune_result() {
        let msg = DaemonMessage::PruneResult {
            request_id: "pr-1".to_string(),
            pruned: vec!["feat/old".to_string()],
            errors: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, DaemonMessage::PruneResult { pruned, errors, .. }
                if pruned.len() == 1 && errors.is_empty())
        );
    }

    #[test]
    fn roundtrip_task_run_request() {
        let msg = AgentMessage::TaskRunRequest {
            request_id: "tr-1".to_string(),
            branch: "feat/auth".to_string(),
            command: vec!["claude".to_string(), "-p".to_string(), "test".to_string()],
            base: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"task_run_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::TaskRunRequest { branch, .. } if branch == "feat/auth"
        ));
    }

    #[test]
    fn roundtrip_task_list_result() {
        let msg = DaemonMessage::TaskListResult {
            request_id: "tl-1".to_string(),
            tasks: vec![TaskEntry {
                task_id: "feat-auth".to_string(),
                branch: "feat/auth".to_string(),
                container_name: "cella-proj-feat-auth".to_string(),
                status: TaskStatus::Running,
                command: vec!["claude".to_string()],
                elapsed_secs: 120,
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonMessage::TaskListResult { tasks, .. } if tasks.len() == 1));
    }

    #[test]
    fn roundtrip_task_wait_result() {
        let msg = DaemonMessage::TaskWaitResult {
            request_id: "tw-1".to_string(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::TaskWaitResult { exit_code: 0, .. }
        ));
    }
}
