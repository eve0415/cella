//! IPC protocol types shared between cella-agent, cella-daemon, and cella-cli.
//!
//! Agent↔daemon messages are newline-delimited JSON over TCP.
//! Management messages (CLI↔daemon) use the same framing over a Unix socket.

pub mod credential;
pub mod credential_frame;

use serde::{Deserialize, Serialize};

/// Current protocol version for the agent↔daemon handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Sent by an agent on a new TCP connection to identify it as a reverse tunnel
/// for an active port-forward. Discriminated from [`AgentHello`] by the
/// `connection_id` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelHandshake {
    pub auth_token: String,
    pub connection_id: u64,
}

/// Credential proxy handshake sent by the agent.
///
/// Discriminated from [`TunnelHandshake`] by the `provider_id` field
/// and from [`AgentHello`] by the `request_id` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialProxyHandshake {
    pub auth_token: String,
    pub container_name: String,
    pub request_id: String,
    pub domain: String,
    pub provider_id: String,
    /// Per-container nonce for tunnel authentication (replaces global auth token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_nonce: Option<String>,
    /// Unique identifier for audit logging and request correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// A phantom token entry mapping a provider to its opaque replacement value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhantomTokenEntry {
    pub provider_id: String,
    pub phantom_token: String,
    pub env_var: String,
    pub domains: Vec<String>,
    /// HTTP header name for credential injection.
    #[serde(default)]
    pub header: String,
    /// Header value prefix (e.g., `"Bearer "`).
    #[serde(default)]
    pub prefix: String,
}

/// Sent by the agent as the first message after connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    pub protocol_version: u32,
    pub agent_version: String,
    /// Container name for routing (agent self-identifies).
    pub container_name: String,
    /// Auth token for validating the connection.
    pub auth_token: String,
    /// Whether this agent participates in `~/.claude.json` bidirectional sync
    /// (set from `CELLA_SYNC_CLAUDE_CONFIG`). The daemon only broadcasts config
    /// updates to agents that advertise this. Defaults to false for older
    /// agents that don't send the field.
    #[serde(default)]
    pub claude_config_sync: bool,
    /// One-shot connections (browser-open, credential, clipboard) set this so
    /// the daemon skips `agent_tx` and connection-state management.
    #[serde(default)]
    pub transient: bool,
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
    /// Copy data to the host clipboard.
    ClipboardCopy {
        /// Base64-encoded clipboard content.
        data: String,
        /// MIME type of the content (e.g. "text/plain", "image/png").
        mime_type: String,
    },
    /// Request clipboard content from the host.
    ClipboardPaste {
        /// Requested MIME type (None = default text/plain).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mime_type: Option<String>,
    },
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
    /// The container's `~/.claude.json` changed; carries its full content for
    /// the daemon to deep-merge into the canonical config and propagate.
    ClaudeConfigChanged { content: String },

    // -- Worktree operations (in-container CLI → daemon) --------------------
    /// Request to create a worktree-backed branch and its container.
    BranchRequest {
        request_id: String,
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
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
    /// Remove worktrees and their containers.
    PruneRequest {
        request_id: String,
        #[serde(default)]
        dry_run: bool,
        /// When true, include unmerged worktrees (not just merged ones).
        #[serde(default)]
        all: bool,
        /// Only prune worktrees older than this duration (e.g. "7d", "24h").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        older_than: Option<String>,
        /// Only prune worktrees whose git branch no longer exists.
        #[serde(default)]
        missing_worktree: bool,
        /// Only prune worktrees matching these labels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
    },
    /// Stop (and optionally remove) a worktree branch's container.
    DownRequest {
        request_id: String,
        /// Branch name whose container to stop.
        branch: String,
        /// Remove the container and worktree directory after stopping.
        #[serde(default)]
        rm: bool,
        /// Remove associated volumes (only with rm).
        #[serde(default)]
        volumes: bool,
        /// Force stop even when shutdownAction is "none".
        #[serde(default)]
        force: bool,
    },
    /// Start or restart a worktree branch's container.
    UpRequest {
        request_id: String,
        /// Branch name whose container to start.
        branch: String,
        /// Rebuild the container from scratch.
        #[serde(default)]
        rebuild: bool,
    },
    /// Create a branch and run a background command in its container.
    TaskRunRequest {
        request_id: String,
        branch: String,
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// List active background tasks.
    TaskListRequest { request_id: String },
    /// Stream output from a background task.
    TaskLogsRequest {
        request_id: String,
        branch: String,
        #[serde(default)]
        follow: bool,
    },
    /// Block until a background task completes.
    TaskWaitRequest { request_id: String, branch: String },
    /// Stop a running background task.
    TaskStopRequest { request_id: String, branch: String },
    /// Execute a command and capture stdout/stderr separately (JSON mode).
    ExecCaptureRequest {
        request_id: String,
        branch: String,
        command: Vec<String>,
    },
    /// Request structured health/status data.
    DoctorRequest { request_id: String },
    /// Switch to another branch's container (run default shell).
    SwitchRequest { request_id: String, branch: String },
}

// ---------------------------------------------------------------------------
// Management protocol (CLI ↔ daemon via ~/.cella/daemon.sock)
// ---------------------------------------------------------------------------

/// Data for a container registration request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRegistrationData {
    pub container_id: String,
    pub container_name: String,
    pub container_ip: Option<String>,
    pub ports_attributes: Vec<PortAttributes>,
    pub other_ports_attributes: Option<PortAttributes>,
    /// Ports from `forwardPorts` in devcontainer.json (pre-allocate on registration).
    #[serde(default)]
    pub forward_ports: Vec<u16>,
    /// The `shutdownAction` from devcontainer.json (`"none"` or `"stopContainer"`).
    #[serde(default)]
    pub shutdown_action: Option<String>,
    /// Which backend created this container (e.g. `"docker"`, `"apple-container"`).
    #[serde(default)]
    pub backend_kind: Option<String>,
    /// Docker host override used when the container was created.
    #[serde(default)]
    pub docker_host: Option<String>,
    /// Project name from devcontainer.json `name` field or repo directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// Git branch name (for worktree containers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Requests from CLI tools to the daemon management socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagementRequest {
    /// Register a new container for port management.
    RegisterContainer(Box<ContainerRegistrationData>),
    /// Deregister a container (stop proxies, release ports).
    DeregisterContainer { container_name: String },
    /// Query all forwarded ports across containers.
    QueryPorts,
    /// Query daemon status.
    QueryStatus,
    /// Health check.
    Ping,
    /// Update a container's IP address after it has started.
    ///
    /// Sent after pre-registration (with `container_ip: None`) once the
    /// container is running and its IP is known.
    UpdateContainerIp {
        container_id: String,
        container_ip: Option<String>,
    },
    /// Register a per-workspace SSH-agent proxy. The daemon binds a Unix
    /// socket under `~/.cella/run/`, bidirectionally forwards bytes to
    /// `upstream_socket` (the host's `$SSH_AUTH_SOCK`), and returns the
    /// proxy socket path that the caller should bind-mount into the
    /// container. Refcounted by `workspace`: subsequent registrations for
    /// the same workspace bump the count and return the existing socket.
    RegisterSshAgentProxy {
        workspace: String,
        upstream_socket: String,
    },
    /// Decrement the SSH-agent proxy refcount for `workspace`. Tears down
    /// the listener and unlinks the socket file when the count reaches
    /// zero. No-op for an unknown workspace.
    ReleaseSshAgentProxy { workspace: String },
    /// Re-validate the SSH-agent bridge for `workspace` against the
    /// caller's current `upstream_socket` without changing the refcount.
    /// Heals bridges whose registered upstream went stale (host agent
    /// moved after sleep/re-login/agent restart): same upstream is a
    /// no-op, a different one rebinds on the same port when possible,
    /// and a missing entry creates a fresh bridge.
    RefreshSshAgentProxy {
        workspace: String,
        upstream_socket: String,
    },
    /// Register phantom tokens for a credential-protected container.
    RegisterPhantomTokens {
        container_name: String,
        tokens: Vec<PhantomTokenEntry>,
    },
    /// Retrieve phantom token values for a container (used at exec time).
    GetPhantomTokens { container_name: String },
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
        /// Hostname proxy bind state for diagnostics and URL rendering.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname_proxy: Option<HostnameProxyStatus>,
    },
    /// Daemon is shutting down.
    ShuttingDown { pid: u32 },
    /// Container IP updated.
    ContainerIpUpdated { container_id: String },
    /// Pong response.
    Pong,
    /// Phantom tokens registered for a container.
    PhantomTokensRegistered {
        container_name: String,
        /// Per-container nonce for credential tunnel authentication.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        container_nonce: Option<String>,
    },
    /// Phantom token values for exec-time injection.
    PhantomTokenValues {
        /// Map of env var name → phantom token value.
        tokens: std::collections::HashMap<String, String>,
    },
    /// SSH-agent bridge registered (or refcount bumped). `bridge_port` is
    /// the localhost TCP port the in-container `cella-agent` should
    /// connect to (reachable from the container as `host.docker.internal`,
    /// `host.container.internal`, or the equivalent host-gateway hostname). `refcount`
    /// is the post-register count; `1` means a fresh bridge was created,
    /// `>1` means an existing one was reused.
    SshAgentProxyRegistered { bridge_port: u16, refcount: usize },
    /// SSH-agent proxy refcount decremented. `torn_down` is true when the
    /// refcount reached zero and the listener was actually destroyed; false
    /// when the proxy is still in use by another container in the same
    /// workspace, or when the workspace was never registered.
    SshAgentProxyReleased { torn_down: bool },
    /// SSH-agent bridge re-validated. `bridge_port` is the (possibly
    /// rebound) localhost TCP port; `refcount` is unchanged by refresh.
    SshAgentProxyRefreshed {
        bridge_port: u16,
        refcount: usize,
        action: SshProxyRefreshAction,
        /// Whether the upstream socket accepted a probe connection.
        /// `Some(false)` means the host agent itself is down — bridged
        /// traffic will EOF even though the bridge is healthy. `None`
        /// from daemons that don't probe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upstream_reachable: Option<bool>,
        /// `true` when a rebridge could not reclaim the previous port —
        /// containers carry the old port baked into their env and need a
        /// rebuild to reach the bridge again. Defaults to `false` from
        /// daemons that predate the field.
        #[serde(default)]
        port_changed: bool,
    },
    /// Error response.
    Error { message: String },
}

/// What a `RefreshSshAgentProxy` request actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshProxyRefreshAction {
    /// Bridge already pointed at the requested upstream.
    Unchanged,
    /// Stale upstream — bridge was torn down and rebound.
    Rebridged,
    /// No bridge existed — a fresh one was created.
    Created,
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
    /// Hostname-based URL (e.g., `http://3000.main.myapp.localhost`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// Runtime state for the hostname HTTP proxy listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostnameProxyStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub using_fallback_port: bool,
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
    /// Agent version from the `AgentHello` handshake, if connected.
    #[serde(default)]
    pub agent_version: Option<String>,
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
    /// Clipboard content response.
    ClipboardContent {
        /// Base64-encoded clipboard content.
        data: String,
        /// MIME type of the content.
        mime_type: String,
    },
    /// Configuration update from the daemon.
    Config {
        poll_interval_ms: u64,
        proxy_localhost: bool,
    },
    /// Port mapping notification: tells the agent which host port was allocated.
    PortMapping { container_port: u16, host_port: u16 },
    /// Request the agent to open a reverse tunnel for a new forwarded connection.
    TunnelRequest {
        connection_id: u64,
        target_port: u16,
    },
    /// Push the canonical `~/.claude.json` content to the agent so it can write
    /// it into the container. Only sent to agents that advertised
    /// `claude_config_sync` in their `AgentHello`.
    SyncClaudeConfig { content: String },

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
    /// Result of a down (stop/remove) request.
    DownResult {
        request_id: String,
        #[serde(flatten)]
        result: DownOperationResult,
    },
    /// Result of an up (start/restart) request.
    UpResult {
        request_id: String,
        #[serde(flatten)]
        result: WorktreeOperationResult,
    },
    /// A background task was started.
    TaskRunResult {
        request_id: String,
        #[serde(flatten)]
        result: TaskRunOperationResult,
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
    /// Keep-alive during `task wait` so the agent knows the connection is alive.
    TaskWaitHeartbeat { request_id: String },
    /// Result of a captured exec request (stdout/stderr separated).
    ExecCaptureResult {
        request_id: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    /// Background task stopped.
    TaskStopResult { request_id: String },
    /// Stream channel is ready for TTY forwarding.
    StreamReady { request_id: String, port: u16 },
    /// Structured health/status data.
    DoctorResult {
        request_id: String,
        data: serde_json::Value,
    },
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
///
/// Keys in `portsAttributes` can be:
/// - `"3000"` — exact port number → [`Single`](PortPattern::Single)
/// - `"3000-3010"` — inclusive range → [`Range`](PortPattern::Range)
/// - `"hostname:3000"` — host-qualified port; cella matches on port number only → [`HostPort`](PortPattern::HostPort)
/// - anything else — treated as a Rust regex (RE2-like syntax) tested
///   against the port number string (e.g. `"^30\\d\\d$"` matches 3000–3099).
///
/// ## Regex semantics
///
/// The devcontainer spec (VS Code `tunnelModel.ts`) tests regex keys against the
/// **process command line** of the port.  cella's daemon does not yet expose
/// per-port process metadata at match time, so we test against the decimal
/// port-number string instead (e.g. port 3000 → `"3000"`).
///
/// Note: the Rust `regex` crate uses RE2-like syntax — look-around and
/// backreferences are not supported.
///
/// Invalid regex patterns compile to a never-matching variant — they never panic.
///
/// ## Serialization
///
/// Serializes with an adjacent tag (`{"type":"single","value":3000}`).
/// Deserialization additionally accepts the legacy externally-tagged format
/// (`{"Single":3000}`) written by cella versions prior to `HostPort`/`Regex`
/// support, so existing container labels remain readable after an upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PortPattern {
    /// Exact port number (e.g. `"3000"`).
    Single(u16),
    /// Inclusive port range (e.g. `"3000-3010"`).
    Range(u16, u16),
    /// Host-qualified port key (e.g. `"db:5432"`).  Matched on port number only.
    HostPort { host: String, port: u16 },
    /// Rust RE2-like regex pattern tested against the decimal port-number string.
    ///
    /// Stores the original source pattern so the value survives serde round-trips.
    /// An invalid pattern stored here will always return `false` from [`matches`](Self::matches).
    Regex(String),
}

impl<'de> serde::Deserialize<'de> for PortPattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;

        // Try the current adjacent-tag format first.
        if let Ok(p) = serde_json::from_value::<PortPatternTagged>(raw.clone()) {
            return Ok(p.into());
        }

        // Fall back to the legacy external-tag format written before HostPort/Regex
        // were introduced: {"Single":3000} or {"Range":[3000,3010]}.
        serde_json::from_value::<PortPatternLegacy>(raw)
            .map(Into::into)
            .map_err(serde::de::Error::custom)
    }
}

/// Internal helper — mirrors `PortPattern` with the adjacent-tag serde layout.
#[derive(serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum PortPatternTagged {
    Single(u16),
    Range(u16, u16),
    HostPort { host: String, port: u16 },
    Regex(String),
}

impl From<PortPatternTagged> for PortPattern {
    fn from(t: PortPatternTagged) -> Self {
        match t {
            PortPatternTagged::Single(p) => Self::Single(p),
            PortPatternTagged::Range(lo, hi) => Self::Range(lo, hi),
            PortPatternTagged::HostPort { host, port } => Self::HostPort { host, port },
            PortPatternTagged::Regex(s) => Self::Regex(s),
        }
    }
}

/// Legacy external-tag format: `{"Single":3000}` / `{"Range":[3000,3010]}`.
/// Only `Single` and `Range` existed before `HostPort`/`Regex` were added.
#[derive(serde::Deserialize)]
enum PortPatternLegacy {
    Single(u16),
    Range(u16, u16),
}

impl From<PortPatternLegacy> for PortPattern {
    fn from(l: PortPatternLegacy) -> Self {
        match l {
            PortPatternLegacy::Single(p) => Self::Single(p),
            PortPatternLegacy::Range(lo, hi) => Self::Range(lo, hi),
        }
    }
}

impl PortPattern {
    /// Check if a port number matches this pattern.
    ///
    /// For [`Regex`](Self::Regex): tests the compiled regex against the decimal
    /// string of `port`.  An invalid stored pattern never matches (returns `false`).
    pub fn matches(&self, port: u16) -> bool {
        match self {
            Self::Single(p) | Self::HostPort { port: p, .. } => *p == port,
            Self::Range(lo, hi) => port >= *lo && port <= *hi,
            Self::Regex(pattern) => {
                regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&port.to_string()))
            }
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

/// Outcome of a container stop/remove operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownOutcome {
    Stopped,
    Removed,
}

/// Result of a down (stop/remove) operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DownOperationResult {
    Success {
        outcome: DownOutcome,
        container_name: String,
    },
    Error {
        message: String,
    },
}

/// Result of a task run operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskRunOperationResult {
    Success {
        task_id: String,
        container_name: String,
    },
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<TaskErrorCode>,
    },
}

/// Structured error codes for task operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskErrorCode {
    AlreadyRunning,
    NotFound,
    BranchNotFound,
    ContainerNotRunning,
    ExecFailed,
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
    TimedOut,
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
    /// Docker container ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    /// Container labels (all labels, including cella-managed ones).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<std::collections::HashMap<String, String>>,
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
    fn roundtrip_clipboard_copy() {
        let msg = AgentMessage::ClipboardCopy {
            data: "aGVsbG8gd29ybGQ=".to_string(),
            mime_type: "text/plain".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"clipboard_copy\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::ClipboardCopy { mime_type, .. } if mime_type == "text/plain")
        );
    }

    #[test]
    fn roundtrip_clipboard_paste() {
        let msg = AgentMessage::ClipboardPaste {
            mime_type: Some("text/plain".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"clipboard_paste\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::ClipboardPaste { mime_type: Some(ref m), .. } if m == "text/plain")
        );
    }

    #[test]
    fn roundtrip_clipboard_paste_no_mime() {
        let msg = AgentMessage::ClipboardPaste { mime_type: None };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("mime_type"));
    }

    #[test]
    fn roundtrip_clipboard_content() {
        let msg = DaemonMessage::ClipboardContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "text/plain".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"clipboard_content\""));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonMessage::ClipboardContent { .. }));
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
    fn roundtrip_tunnel_handshake() {
        let hs = TunnelHandshake {
            auth_token: "secret".to_string(),
            connection_id: 42,
        };
        let json = serde_json::to_string(&hs).unwrap();
        assert!(json.contains("\"connection_id\":42"));
        assert!(json.contains("\"auth_token\":\"secret\""));
        let decoded: TunnelHandshake = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.connection_id, 42);
        assert_eq!(decoded.auth_token, "secret");
    }

    #[test]
    fn roundtrip_tunnel_request() {
        let msg = DaemonMessage::TunnelRequest {
            connection_id: 99,
            target_port: 3000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"tunnel_request\""));
        assert!(json.contains("\"connection_id\":99"));
        assert!(json.contains("\"target_port\":3000"));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::TunnelRequest {
                connection_id: 99,
                target_port: 3000
            }
        ));
    }

    #[test]
    fn tunnel_handshake_not_parseable_as_agent_hello() {
        let hs = TunnelHandshake {
            auth_token: "token".to_string(),
            connection_id: 1,
        };
        let json = serde_json::to_string(&hs).unwrap();
        let result = serde_json::from_str::<AgentHello>(&json);
        assert!(result.is_err());
    }

    #[test]
    fn agent_hello_not_parseable_as_tunnel_handshake() {
        let hello = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            claude_config_sync: false,
            transient: false,
        };
        let json = serde_json::to_string(&hello).unwrap();
        let result = serde_json::from_str::<TunnelHandshake>(&json);
        assert!(result.is_err());
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
    fn port_pattern_host_port_match() {
        let p = PortPattern::HostPort {
            host: "db".to_string(),
            port: 5432,
        };
        assert!(p.matches(5432));
        assert!(!p.matches(5433));
    }

    #[test]
    fn port_pattern_regex_anchored_match() {
        // "^30\d\d$" matches 3000–3099, not 4000, not 30000
        let p = PortPattern::Regex(r"^30\d\d$".to_string());
        assert!(p.matches(3000));
        assert!(p.matches(3099));
        assert!(!p.matches(4000));
        assert!(!p.matches(30_000));
    }

    #[test]
    fn port_pattern_invalid_regex_no_panic() {
        let p = PortPattern::Regex("[bad".to_string());
        assert!(!p.matches(3000));
    }

    #[test]
    fn port_pattern_regex_serde_roundtrip() {
        let p = PortPattern::Regex(r"^30\d\d$".to_string());
        let json = serde_json::to_string(&p).unwrap();
        let p2: PortPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
        assert!(p2.matches(3000));
    }

    #[test]
    fn port_protocol_display() {
        assert_eq!(PortProtocol::Tcp.to_string(), "tcp");
        assert_eq!(PortProtocol::Udp.to_string(), "udp");
    }

    #[test]
    fn serialize_management_register() {
        let req = ManagementRequest::RegisterContainer(Box::new(ContainerRegistrationData {
            container_id: "abc123".to_string(),
            container_name: "cella-myapp-main".to_string(),
            container_ip: Some("172.20.0.5".to_string()),
            ports_attributes: vec![],
            other_ports_attributes: None,
            forward_ports: vec![],
            shutdown_action: None,
            backend_kind: Some("docker".to_string()),
            docker_host: None,
            project_name: None,
            branch: None,
        }));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"register_container\""));
        assert!(json.contains("\"container_ip\":\"172.20.0.5\""));
    }

    #[test]
    fn deserialize_register_without_backend_fields() {
        // Backward compatibility: old CLI versions may omit backend_kind/docker_host.
        let json = r#"{"type":"register_container","container_id":"abc","container_name":"test","container_ip":null,"ports_attributes":[],"other_ports_attributes":null,"forward_ports":[],"shutdown_action":null}"#;
        let req: ManagementRequest = serde_json::from_str(json).unwrap();
        if let ManagementRequest::RegisterContainer(data) = req {
            assert!(data.backend_kind.is_none());
            assert!(data.docker_host.is_none());
        } else {
            panic!("expected RegisterContainer");
        }
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
                hostname: None,
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
                agent_version: Some("0.1.0".to_string()),
            }],
            is_orbstack: false,
            daemon_version: "0.1.0".to_string(),
            daemon_started_at: 1_700_000_000,
            control_port: 54321,
            control_token: "abc123".to_string(),
            hostname_proxy: Some(HostnameProxyStatus {
                enabled: true,
                address: Some("127.0.0.1:49180".to_string()),
                port: Some(49180),
                using_fallback_port: true,
            }),
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
            hostname_proxy,
            ..
        } = decoded
        {
            assert!(daemon_version.is_empty());
            assert_eq!(daemon_started_at, 0);
            assert!(hostname_proxy.is_none());
        } else {
            panic!("Expected Status");
        }
    }

    #[test]
    fn roundtrip_claude_config_changed() {
        let msg = AgentMessage::ClaudeConfigChanged {
            content: r#"{"numStartups":3}"#.to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"claude_config_changed\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::ClaudeConfigChanged { content } if content == r#"{"numStartups":3}"#)
        );
    }

    #[test]
    fn roundtrip_sync_claude_config() {
        let msg = DaemonMessage::SyncClaudeConfig {
            content: r#"{"mcpServers":{}}"#.to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"sync_claude_config\""));
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, DaemonMessage::SyncClaudeConfig { content } if content == r#"{"mcpServers":{}}"#)
        );
    }

    #[test]
    fn agent_hello_claude_config_sync_roundtrip() {
        let hello = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            claude_config_sync: true,
            transient: false,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"claude_config_sync\":true"));
        let decoded: AgentHello = serde_json::from_str(&json).unwrap();
        assert!(decoded.claude_config_sync);
    }

    #[test]
    fn agent_hello_backward_compat_missing_claude_config_sync() {
        // Old agents won't send the flag; it must default to false.
        let json = r#"{"protocol_version":1,"agent_version":"0.1.0","container_name":"c","auth_token":"t"}"#;
        let decoded: AgentHello = serde_json::from_str(json).unwrap();
        assert!(!decoded.claude_config_sync);
    }

    #[test]
    fn agent_hello_transient_flag_roundtrip() {
        let hello = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            claude_config_sync: false,
            transient: true,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"transient\":true"));
        let decoded: AgentHello = serde_json::from_str(&json).unwrap();
        assert!(decoded.transient);
    }

    #[test]
    fn agent_hello_backward_compat_missing_transient() {
        // Old agents won't send transient; it must default to false.
        let json = r#"{"protocol_version":1,"agent_version":"0.1.0","container_name":"c","auth_token":"t"}"#;
        let decoded: AgentHello = serde_json::from_str(json).unwrap();
        assert!(!decoded.transient);
    }

    #[test]
    fn roundtrip_agent_hello() {
        let hello = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            container_name: "test-container".to_string(),
            auth_token: "token123".to_string(),
            claude_config_sync: false,
            transient: false,
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
    fn roundtrip_register_ssh_agent_proxy_request() {
        let req = ManagementRequest::RegisterSshAgentProxy {
            workspace: "/Users/me/proj".to_string(),
            upstream_socket: "/Users/me/.1password/agent.sock".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"register_ssh_agent_proxy\""));
        let decoded: ManagementRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementRequest::RegisterSshAgentProxy { ref workspace, ref upstream_socket }
                if workspace == "/Users/me/proj"
                    && upstream_socket == "/Users/me/.1password/agent.sock"
        ));
    }

    #[test]
    fn roundtrip_release_ssh_agent_proxy_request() {
        let req = ManagementRequest::ReleaseSshAgentProxy {
            workspace: "/Users/me/proj".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"release_ssh_agent_proxy\""));
        let decoded: ManagementRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementRequest::ReleaseSshAgentProxy { ref workspace }
                if workspace == "/Users/me/proj"
        ));
    }

    #[test]
    fn roundtrip_ssh_agent_proxy_registered_response() {
        let resp = ManagementResponse::SshAgentProxyRegistered {
            bridge_port: 54321,
            refcount: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"ssh_agent_proxy_registered\""));
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementResponse::SshAgentProxyRegistered { refcount: 1, .. }
        ));
    }

    #[test]
    fn roundtrip_ssh_agent_proxy_released_response() {
        for torn_down in [true, false] {
            let resp = ManagementResponse::SshAgentProxyReleased { torn_down };
            let json = serde_json::to_string(&resp).unwrap();
            assert!(json.contains("\"type\":\"ssh_agent_proxy_released\""));
            let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
            assert!(matches!(
                decoded,
                ManagementResponse::SshAgentProxyReleased { torn_down: t } if t == torn_down
            ));
        }
    }

    #[test]
    fn roundtrip_refresh_ssh_agent_proxy_request() {
        let req = ManagementRequest::RefreshSshAgentProxy {
            workspace: "/Users/me/proj".to_string(),
            upstream_socket: "/private/tmp/com.apple.launchd.abc/Listeners".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"refresh_ssh_agent_proxy\""));
        let decoded: ManagementRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementRequest::RefreshSshAgentProxy { ref workspace, ref upstream_socket }
                if workspace == "/Users/me/proj"
                    && upstream_socket == "/private/tmp/com.apple.launchd.abc/Listeners"
        ));
    }

    #[test]
    fn roundtrip_ssh_agent_proxy_refreshed_response() {
        for (action, wire) in [
            (SshProxyRefreshAction::Unchanged, "\"action\":\"unchanged\""),
            (SshProxyRefreshAction::Rebridged, "\"action\":\"rebridged\""),
            (SshProxyRefreshAction::Created, "\"action\":\"created\""),
        ] {
            let resp = ManagementResponse::SshAgentProxyRefreshed {
                bridge_port: 54321,
                refcount: 2,
                action,
                upstream_reachable: Some(true),
                port_changed: false,
            };
            let json = serde_json::to_string(&resp).unwrap();
            assert!(json.contains("\"type\":\"ssh_agent_proxy_refreshed\""));
            assert!(json.contains(wire));
            let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
            assert!(matches!(
                decoded,
                ManagementResponse::SshAgentProxyRefreshed {
                    bridge_port: 54321,
                    refcount: 2,
                    action: a,
                    upstream_reachable: Some(true),
                    port_changed: false,
                } if a == action
            ));
        }
    }

    #[test]
    fn ssh_agent_proxy_refreshed_tolerates_missing_optional_fields() {
        // Older daemons don't send the probe or port-change fields — they
        // must decode as None / false.
        let json = r#"{"type":"ssh_agent_proxy_refreshed","bridge_port":1,"refcount":1,"action":"unchanged"}"#;
        let decoded: ManagementResponse = serde_json::from_str(json).unwrap();
        assert!(matches!(
            decoded,
            ManagementResponse::SshAgentProxyRefreshed {
                upstream_reachable: None,
                port_changed: false,
                ..
            }
        ));
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
            labels: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"branch_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, AgentMessage::BranchRequest { request_id, branch, base, .. }
                if request_id == "br-1" && branch == "feat/auth" && base.as_deref() == Some("main"))
        );
    }

    #[test]
    fn roundtrip_branch_request_no_base() {
        let msg = AgentMessage::BranchRequest {
            request_id: "br-2".to_string(),
            branch: "fix/bug".to_string(),
            base: None,
            labels: None,
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
                    container_id: None,
                    labels: None,
                },
                WorktreeEntry {
                    branch: Some("feat/auth".to_string()),
                    worktree_path: "/home/user/project-worktrees/feat-auth".to_string(),
                    is_main: false,
                    container_name: None,
                    container_state: None,
                    container_id: None,
                    labels: None,
                },
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, DaemonMessage::ListResult { worktrees, .. } if worktrees.len() == 2)
        );
    }

    #[test]
    fn worktree_entry_backward_compat_missing_new_fields() {
        let json = r#"{"branch":"main","worktree_path":"/repo","is_main":true,"container_name":"ctr","container_state":"running"}"#;
        let entry: WorktreeEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.branch.as_deref(), Some("main"));
        assert!(entry.container_id.is_none());
        assert!(entry.labels.is_none());
    }

    #[test]
    fn worktree_entry_new_fields_roundtrip() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("team".to_string(), "backend".to_string());
        let entry = WorktreeEntry {
            branch: Some("feat/x".to_string()),
            worktree_path: "/repo".to_string(),
            is_main: false,
            container_name: Some("ctr".to_string()),
            container_state: Some("running".to_string()),
            container_id: Some("abc123def456".to_string()),
            labels: Some(labels),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"container_id\":\"abc123def456\""));
        assert!(json.contains("\"team\":\"backend\""));
        let decoded: WorktreeEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.container_id.as_deref(), Some("abc123def456"));
        assert_eq!(
            decoded.labels.as_ref().unwrap().get("team").unwrap(),
            "backend"
        );
    }

    #[test]
    fn worktree_entry_none_fields_omitted_in_json() {
        let entry = WorktreeEntry {
            branch: None,
            worktree_path: "/repo".to_string(),
            is_main: false,
            container_name: None,
            container_state: None,
            container_id: None,
            labels: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("container_id"));
        assert!(!json.contains("labels"));
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
            all: false,
            older_than: None,
            missing_worktree: false,
            labels: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::PruneRequest { dry_run: true, .. }
        ));
    }

    #[test]
    fn roundtrip_prune_request_all() {
        let msg = AgentMessage::PruneRequest {
            request_id: "pr-2".to_string(),
            dry_run: false,
            all: true,
            older_than: None,
            missing_worktree: false,
            labels: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::PruneRequest { all: true, .. }
        ));
    }

    #[test]
    fn prune_request_backward_compat_missing_all() {
        let json = r#"{"type":"prune_request","request_id":"pr-3","dry_run":false}"#;
        let decoded: AgentMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::PruneRequest { all: false, .. }
        ));
    }

    #[test]
    fn roundtrip_down_request() {
        let msg = AgentMessage::DownRequest {
            request_id: "dn-1".to_string(),
            branch: "feat/auth".to_string(),
            rm: true,
            volumes: false,
            force: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"down_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::DownRequest { rm: true, .. }
        ));
    }

    #[test]
    fn roundtrip_up_request() {
        let msg = AgentMessage::UpRequest {
            request_id: "up-1".to_string(),
            branch: "feat/auth".to_string(),
            rebuild: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"up_request\""));
        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentMessage::UpRequest { rebuild: true, .. }
        ));
    }

    #[test]
    fn roundtrip_down_result() {
        let msg = DaemonMessage::DownResult {
            request_id: "dn-1".to_string(),
            result: DownOperationResult::Success {
                outcome: DownOutcome::Stopped,
                container_name: "cella-proj-feat-auth".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::DownResult {
                result: DownOperationResult::Success {
                    outcome: DownOutcome::Stopped,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn roundtrip_up_result() {
        let msg = DaemonMessage::UpResult {
            request_id: "up-1".to_string(),
            result: WorktreeOperationResult::Success {
                container_name: "cella-proj-feat-auth".to_string(),
                worktree_path: "/home/user/proj-worktrees/feat-auth".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: DaemonMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            DaemonMessage::UpResult {
                result: WorktreeOperationResult::Success { .. },
                ..
            }
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
            timeout_secs: None,
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

    // -- Credential proxy protocol tests --------------------------------------

    #[test]
    fn roundtrip_credential_proxy_handshake() {
        let hs = CredentialProxyHandshake {
            auth_token: "tok".to_string(),
            container_name: "cella-proj-main".to_string(),
            request_id: "req-1".to_string(),
            domain: "api.anthropic.com".to_string(),
            provider_id: "anthropic".to_string(),
            container_nonce: Some("abc123".to_string()),
            trace_id: Some("cred-550e8400".to_string()),
        };
        let json = serde_json::to_string(&hs).unwrap();
        let decoded: CredentialProxyHandshake = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.provider_id, "anthropic");
        assert_eq!(decoded.domain, "api.anthropic.com");
        assert_eq!(decoded.request_id, "req-1");
        assert_eq!(decoded.container_nonce.as_deref(), Some("abc123"));
        assert_eq!(decoded.trace_id.as_deref(), Some("cred-550e8400"));
    }

    #[test]
    fn credential_proxy_handshake_backward_compat() {
        let json = r#"{"auth_token":"tok","container_name":"c","request_id":"r","domain":"d","provider_id":"p"}"#;
        let decoded: CredentialProxyHandshake = serde_json::from_str(json).unwrap();
        assert!(decoded.container_nonce.is_none());
        assert!(decoded.trace_id.is_none());
    }

    #[test]
    fn credential_proxy_handshake_nonce_omitted_when_none() {
        let hs = CredentialProxyHandshake {
            auth_token: "tok".to_string(),
            container_name: "c".to_string(),
            request_id: "r".to_string(),
            domain: "d".to_string(),
            provider_id: "p".to_string(),
            container_nonce: None,
            trace_id: None,
        };
        let json = serde_json::to_string(&hs).unwrap();
        assert!(!json.contains("container_nonce"));
        assert!(!json.contains("trace_id"));
    }

    #[test]
    fn credential_proxy_handshake_not_parseable_as_tunnel() {
        let hs = CredentialProxyHandshake {
            auth_token: "tok".to_string(),
            container_name: "c".to_string(),
            request_id: "r".to_string(),
            domain: "d".to_string(),
            provider_id: "p".to_string(),
            container_nonce: None,
            trace_id: None,
        };
        let json = serde_json::to_string(&hs).unwrap();
        let result = serde_json::from_str::<TunnelHandshake>(&json);
        assert!(result.is_err());
    }

    #[test]
    fn credential_proxy_handshake_not_parseable_as_agent_hello() {
        let hs = CredentialProxyHandshake {
            auth_token: "tok".to_string(),
            container_name: "c".to_string(),
            request_id: "r".to_string(),
            domain: "d".to_string(),
            provider_id: "p".to_string(),
            container_nonce: None,
            trace_id: None,
        };
        let json = serde_json::to_string(&hs).unwrap();
        let result = serde_json::from_str::<AgentHello>(&json);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_phantom_token_entry() {
        let entry = PhantomTokenEntry {
            provider_id: "anthropic".to_string(),
            phantom_token: "pt-550e8400-e29b-41d4-a716-446655440000".to_string(),
            env_var: "ANTHROPIC_API_KEY".to_string(),
            domains: vec!["api.anthropic.com".to_string()],
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: PhantomTokenEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.provider_id, "anthropic");
        assert!(decoded.phantom_token.starts_with("pt-"));
    }

    #[test]
    fn roundtrip_register_phantom_tokens() {
        let req = ManagementRequest::RegisterPhantomTokens {
            container_name: "cella-proj-main".to_string(),
            tokens: vec![PhantomTokenEntry {
                provider_id: "anthropic".to_string(),
                phantom_token: "pt-abc".to_string(),
                env_var: "ANTHROPIC_API_KEY".to_string(),
                domains: vec!["api.anthropic.com".to_string()],
                header: "x-api-key".to_string(),
                prefix: String::new(),
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"register_phantom_tokens\""));
        let decoded: ManagementRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementRequest::RegisterPhantomTokens { .. }
        ));
    }

    #[test]
    fn roundtrip_get_phantom_tokens() {
        let req = ManagementRequest::GetPhantomTokens {
            container_name: "cella-proj-main".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"get_phantom_tokens\""));
        let decoded: ManagementRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementRequest::GetPhantomTokens { .. }
        ));
    }

    #[test]
    fn roundtrip_phantom_tokens_registered() {
        let resp = ManagementResponse::PhantomTokensRegistered {
            container_name: "test".to_string(),
            container_nonce: Some("nonce123".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        if let ManagementResponse::PhantomTokensRegistered {
            container_nonce, ..
        } = decoded
        {
            assert_eq!(container_nonce.as_deref(), Some("nonce123"));
        } else {
            panic!("expected PhantomTokensRegistered");
        }
    }

    #[test]
    fn phantom_tokens_registered_backward_compat() {
        let json = r#"{"type":"phantom_tokens_registered","container_name":"test"}"#;
        let decoded: ManagementResponse = serde_json::from_str(json).unwrap();
        if let ManagementResponse::PhantomTokensRegistered {
            container_nonce, ..
        } = decoded
        {
            assert!(container_nonce.is_none());
        } else {
            panic!("expected PhantomTokensRegistered");
        }
    }

    #[test]
    fn roundtrip_phantom_token_values() {
        let mut tokens = std::collections::HashMap::new();
        tokens.insert("ANTHROPIC_API_KEY".to_string(), "pt-abc-123".to_string());
        tokens.insert("OPENAI_API_KEY".to_string(), "pt-def-456".to_string());
        let resp = ManagementResponse::PhantomTokenValues { tokens };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        if let ManagementResponse::PhantomTokenValues { tokens } = decoded {
            assert_eq!(tokens.len(), 2);
            assert_eq!(tokens["ANTHROPIC_API_KEY"], "pt-abc-123");
        } else {
            panic!("expected PhantomTokenValues");
        }
    }

    #[test]
    fn phantom_token_values_empty_map() {
        let resp = ManagementResponse::PhantomTokenValues {
            tokens: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ManagementResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            ManagementResponse::PhantomTokenValues { tokens } if tokens.is_empty()
        ));
    }

    // ── PortPattern backward-compat deserialization ───────────────────────────

    /// Labels written before `HostPort`/`Regex` were introduced used serde's
    /// default external-tag format: `{"Single":3000}` / `{"Range":[3000,3010]}`.
    /// Upgrading the daemon must not drop those settings.
    #[test]
    fn port_pattern_deserialize_legacy_single() {
        let p: PortPattern = serde_json::from_str(r#"{"Single":3000}"#).unwrap();
        assert_eq!(p, PortPattern::Single(3000));
    }

    #[test]
    fn port_pattern_deserialize_legacy_range() {
        let p: PortPattern = serde_json::from_str(r#"{"Range":[3000,3010]}"#).unwrap();
        assert_eq!(p, PortPattern::Range(3000, 3010));
    }

    /// New format must still round-trip correctly.
    #[test]
    fn port_pattern_serialize_uses_new_format() {
        let p = PortPattern::Single(8080);
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            json.contains("\"type\""),
            "should use adjacent-tag format: {json}"
        );
        let p2: PortPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }
}
