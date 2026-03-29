//! TCP control server: receives messages from in-container agents.
//!
//! A single TCP listener (bound at daemon startup) accepts connections from all
//! containers.  Each agent identifies itself via `AgentHello.container_name` and
//! is validated against the daemon's auth token.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cella_port::protocol::{
    AgentHello, AgentMessage, DaemonHello, DaemonMessage, PROTOCOL_VERSION, PortProtocol,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::CellaDaemonError;
use crate::browser::BrowserHandler;
use crate::credential::invoke_git_credential;
use crate::port_manager::PortManager;
use crate::proxy::ProxyCommand;

/// Shared context for the control server and its connection handlers.
pub(crate) struct ControlContext {
    pub auth_token: String,
    pub port_manager: Arc<Mutex<PortManager>>,
    pub browser_handler: Arc<BrowserHandler>,
    pub container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
    pub proxy_cmd_tx: tokio::sync::mpsc::Sender<ProxyCommand>,
}

/// Tracks whether an agent has actually connected and sent messages.
pub struct AgentConnectionState {
    pub connected: AtomicBool,
    pub last_seen_secs: AtomicU64,
}

impl AgentConnectionState {
    pub const fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            last_seen_secs: AtomicU64::new(0),
        }
    }
}

impl Default for AgentConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for a registered container (no per-container server task).
pub struct ContainerHandle {
    pub container_id: String,
    pub agent_state: Arc<AgentConnectionState>,
}

/// Spawn a handler task for a new agent TCP connection.
fn spawn_agent_handler(stream: tokio::net::TcpStream, ctx: Arc<ControlContext>) {
    tokio::spawn(async move {
        if let Err(e) = handle_agent_connection(stream, &ctx).await {
            warn!("Agent connection error: {e}");
        }
    });
}

/// Handle an accepted TCP connection result.
fn handle_accept_result(
    result: std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
    ctx: &Arc<ControlContext>,
    last_activity: &AtomicU64,
) {
    match result {
        Ok((stream, peer)) => {
            last_activity.store(current_time_secs(), Ordering::Relaxed);
            debug!("Agent TCP connection from {peer}");
            spawn_agent_handler(stream, ctx.clone());
        }
        Err(e) => {
            warn!("Control server accept error: {e}");
        }
    }
}

/// Run the unified TCP control server.
///
/// Accepts agent connections, validates auth tokens, looks up container names
/// in `container_handles`, and routes messages to existing handlers.
pub(crate) async fn run_control_server(
    listener: TcpListener,
    ctx: ControlContext,
    last_activity: Arc<AtomicU64>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let ctx = Arc::new(ctx);

    loop {
        tokio::select! {
            result = listener.accept() => {
                handle_accept_result(result, &ctx, &last_activity);
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("Control server shutting down");
                    break;
                }
            }
        }
    }
}

/// Send a `DaemonHello` rejection and close the connection.
async fn send_reject<W: AsyncWriteExt + Unpin>(writer: &mut W, error: String) {
    let reject = DaemonHello {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        error: Some(error),
        workspace_path: None,
        parent_repo: None,
        is_worktree: false,
    };
    let mut json = serde_json::to_string(&reject).unwrap_or_default();
    json.push('\n');
    let _ = writer.write_all(json.as_bytes()).await;
    let _ = writer.flush().await;
}

/// Validated handshake result from an agent connection.
struct HandshakeResult {
    container_name: String,
    container_id: String,
    container_ip: Option<String>,
    agent_state: Arc<AgentConnectionState>,
}

/// Perform the hello handshake: read `AgentHello`, validate, look up container.
///
/// Returns `Ok(None)` if the connection should be cleanly closed (rejection sent).
/// Returns `Ok(Some(..))` on success with validated handshake data.
async fn perform_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    line: &mut String,
    ctx: &ControlContext,
) -> Result<Option<HandshakeResult>, CellaDaemonError>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let n = reader
        .read_line(line)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("hello read error: {e}"),
        })?;
    if n == 0 {
        return Ok(None);
    }

    let Ok(agent_hello) = serde_json::from_str::<AgentHello>(line.trim()) else {
        send_reject(writer, "Hello required as first message".to_string()).await;
        return Ok(None);
    };

    if agent_hello.protocol_version != PROTOCOL_VERSION {
        send_reject(
            writer,
            format!(
                "protocol version mismatch: agent={} daemon={}",
                agent_hello.protocol_version, PROTOCOL_VERSION
            ),
        )
        .await;
        return Ok(None);
    }

    if agent_hello.auth_token != ctx.auth_token {
        send_reject(writer, "invalid auth token".to_string()).await;
        return Ok(None);
    }

    let container_name = agent_hello.container_name.clone();
    let (container_id, agent_state) = {
        let handles = ctx.container_handles.lock().await;
        if let Some(h) = handles.get(&container_name) {
            (h.container_id.clone(), h.agent_state.clone())
        } else {
            send_reject(writer, format!("unknown container: {container_name}")).await;
            return Ok(None);
        }
    };

    let container_ip = {
        let pm = ctx.port_manager.lock().await;
        pm.container_ip(&container_id).map(String::from)
    };

    if agent_hello.agent_version != env!("CARGO_PKG_VERSION") {
        warn!(
            "Agent version mismatch: agent={} daemon={}",
            agent_hello.agent_version,
            env!("CARGO_PKG_VERSION")
        );
    }

    // Send DaemonHello success
    // TODO(phase-1): look up container labels from Docker to populate workspace metadata
    let hello = DaemonHello {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        error: None,
        workspace_path: None,
        parent_repo: None,
        is_worktree: false,
    };
    let mut json = serde_json::to_string(&hello).map_err(|e| CellaDaemonError::Protocol {
        message: format!("hello serialize error: {e}"),
    })?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("hello write error: {e}"),
        })?;
    writer.flush().await.map_err(|e| CellaDaemonError::Socket {
        message: format!("hello flush error: {e}"),
    })?;

    Ok(Some(HandshakeResult {
        container_name,
        container_id,
        container_ip,
        agent_state,
    }))
}

/// Handle a single agent TCP connection (newline-delimited JSON).
async fn handle_agent_connection(
    stream: tokio::net::TcpStream,
    ctx: &ControlContext,
) -> Result<(), CellaDaemonError> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    let Some(hs) = perform_handshake(&mut reader, &mut writer, &mut line, ctx).await? else {
        return Ok(());
    };

    info!("Agent connected for container {}", hs.container_name);

    // --- Message loop ---
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("read error: {e}"),
            })?;

        if n == 0 {
            break; // Connection closed
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: AgentMessage =
            serde_json::from_str(trimmed).map_err(|e| CellaDaemonError::Protocol {
                message: format!("invalid agent message: {e}"),
            })?;

        // Worktree operations need writer access for multi-message responses.
        // Handle them directly before falling through to the single-response handler.
        match &msg {
            AgentMessage::BranchRequest { .. } | AgentMessage::ListRequest { .. } => {
                handle_worktree_message(msg, &hs.container_name, &mut writer).await?;
                continue;
            }
            _ => {}
        }

        let handler_ctx = AgentHandlerContext {
            port_manager: &ctx.port_manager,
            browser_handler: &ctx.browser_handler,
            container_id: Some(&hs.container_id),
            proxy_cmd_tx: Some(&ctx.proxy_cmd_tx),
            container_ip: hs.container_ip.as_deref(),
        };
        let response = handle_agent_message(msg, &handler_ctx, &hs.agent_state).await;

        if let Some(resp) = response {
            send_message(&mut writer, &resp).await?;
        }
    }

    Ok(())
}

/// Per-connection context shared across agent message handlers.
pub(crate) struct AgentHandlerContext<'a> {
    pub port_manager: &'a Arc<Mutex<PortManager>>,
    pub browser_handler: &'a Arc<BrowserHandler>,
    pub container_id: Option<&'a str>,
    pub proxy_cmd_tx: Option<&'a tokio::sync::mpsc::Sender<ProxyCommand>>,
    pub container_ip: Option<&'a str>,
}

/// Start a TCP proxy for a forwarded port and verify it bound successfully.
///
/// Returns `false` if the proxy bind failed and the allocation should be rolled back.
async fn start_port_proxy(
    host_port: u16,
    container_ip: &str,
    target_port: u16,
    proxy_cmd_tx: &tokio::sync::mpsc::Sender<ProxyCommand>,
) -> bool {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let _ = proxy_cmd_tx
        .send(ProxyCommand::Start {
            host_port,
            container_ip: container_ip.to_string(),
            container_port: target_port,
            result_tx: Some(result_tx),
        })
        .await;

    match result_rx.await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            warn!(
                "Proxy bind failed for port {host_port} (container port {target_port}): {e}. \
                 Rolling back allocation."
            );
            false
        }
        Err(_) => {
            warn!("Proxy coordinator dropped result channel for port {host_port}");
            true
        }
    }
}

/// Handle a `PortOpen` message: allocate a host port, start a proxy, and respond.
async fn handle_port_open(
    port: u16,
    protocol: PortProtocol,
    process: Option<String>,
    proxy_port: Option<u16>,
    ctx: &AgentHandlerContext<'_>,
) -> Option<DaemonMessage> {
    let cid = ctx.container_id.unwrap_or("unknown").to_string();
    debug!(
        "Port open: {port}/{protocol} (process: {process:?}, proxy_port: {proxy_port:?}) from {cid}"
    );
    let host_port = {
        let mut pm = ctx.port_manager.lock().await;
        pm.handle_port_open(&cid, port, protocol, process)
    };

    let target_port = proxy_port.unwrap_or(port);

    if let (Some(hp), Some(tx), Some(ip)) = (host_port, ctx.proxy_cmd_tx, ctx.container_ip)
        && !start_port_proxy(hp, ip, target_port, tx).await
    {
        ctx.port_manager.lock().await.handle_port_closed(&cid, port);
        return None;
    }

    host_port.map(|hp| DaemonMessage::PortMapping {
        container_port: port,
        host_port: hp,
    })
}

/// Handle a port closed message from an agent.
async fn handle_port_closed(port: u16, protocol: PortProtocol, ctx: &AgentHandlerContext<'_>) {
    let cid = ctx.container_id.unwrap_or("unknown").to_string();
    debug!("Port closed: {port}/{protocol} from {cid}");
    let host_port = {
        let mut pm = ctx.port_manager.lock().await;
        pm.handle_port_closed(&cid, port)
    };

    if let (Some(hp), Some(tx)) = (host_port, ctx.proxy_cmd_tx) {
        let _ = tx.send(ProxyCommand::Stop { host_port: hp }).await;
    }
}

/// Handle a browser open request from an agent.
async fn handle_browser_open(url: String, ctx: &AgentHandlerContext<'_>) {
    let rewritten = if let Some(cid) = ctx.container_id {
        rewrite_browser_url(&url, ctx.port_manager, cid).await
    } else {
        url.clone()
    };
    if rewritten == url {
        info!("Browser open request: {url}");
    } else {
        info!("Browser open request: {url} -> {rewritten}");
    }
    if let Some(port) = extract_port(&rewritten) {
        wait_for_proxy_ready(port).await;
    }
    ctx.browser_handler.open_url(&rewritten);
}

/// Handle a credential request from an agent.
async fn handle_credential_request(
    id: String,
    operation: String,
    fields: HashMap<String, String>,
) -> DaemonMessage {
    debug!("Credential request: op={operation} id={id}");
    let result =
        tokio::task::spawn_blocking(move || invoke_git_credential(&operation, &fields)).await;

    let response_fields = match result {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            warn!("Git credential error: {e}");
            HashMap::new()
        }
        Err(e) => {
            warn!("Credential task join error: {e}");
            HashMap::new()
        }
    };
    DaemonMessage::CredentialResponse {
        id,
        fields: response_fields,
    }
}

/// Route an agent message to the appropriate handler.
pub(crate) async fn handle_agent_message(
    msg: AgentMessage,
    ctx: &AgentHandlerContext<'_>,
    agent_state: &Arc<AgentConnectionState>,
) -> Option<DaemonMessage> {
    agent_state.connected.store(true, Ordering::Relaxed);
    agent_state
        .last_seen_secs
        .store(current_time_secs(), Ordering::Relaxed);

    match msg {
        AgentMessage::PortOpen {
            port,
            protocol,
            process,
            proxy_port,
            ..
        } => handle_port_open(port, protocol, process, proxy_port, ctx).await,
        AgentMessage::PortClosed { port, protocol } => {
            handle_port_closed(port, protocol, ctx).await;
            None
        }
        AgentMessage::BrowserOpen { url } => {
            handle_browser_open(url, ctx).await;
            None
        }
        AgentMessage::CredentialRequest {
            id,
            operation,
            fields,
        } => Some(handle_credential_request(id, operation, fields).await),
        AgentMessage::Health {
            uptime_secs,
            ports_detected,
        } => {
            debug!("Agent health: uptime={uptime_secs}s ports={ports_detected}");
            None
        }

        // Worktree operations are handled in the message loop via
        // handle_worktree_message() which has writer access for streaming.
        AgentMessage::BranchRequest { .. } | AgentMessage::ListRequest { .. } => {
            unreachable!("worktree messages intercepted before handle_agent_message")
        }
    }
}

/// Send a single `DaemonMessage` on the writer.
async fn send_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &DaemonMessage,
) -> Result<(), CellaDaemonError> {
    let mut json = serde_json::to_string(msg).map_err(|e| CellaDaemonError::Protocol {
        message: format!("serialize error: {e}"),
    })?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("write error: {e}"),
        })?;
    writer.flush().await.map_err(|e| CellaDaemonError::Socket {
        message: format!("flush error: {e}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Worktree operation handlers
// ---------------------------------------------------------------------------

/// Handle worktree messages that need writer access for multi-message responses.
async fn handle_worktree_message<W: AsyncWriteExt + Unpin>(
    msg: AgentMessage,
    _container_name: &str,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    match msg {
        AgentMessage::BranchRequest {
            request_id,
            branch,
            base,
        } => {
            handle_branch_request(&request_id, &branch, base.as_deref(), writer).await?;
        }
        AgentMessage::ListRequest { request_id } => {
            handle_list_request(&request_id, writer).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Handle a `BranchRequest` by spawning `cella branch` as a subprocess.
///
/// Streams progress and output back to the agent, then sends the final result.
async fn handle_branch_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    base: Option<&str>,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    use cella_port::protocol::{OutputStream, WorktreeOperationResult};
    use tokio::process::Command;

    // Find the cella binary (same binary as the daemon was started from,
    // or fall back to PATH lookup).
    let cella_bin = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            // The daemon is started by the CLI, so the CLI binary is typically
            // a sibling or the same binary. Look for "cella" next to it.
            let dir = exe.parent()?;
            let cella = dir.join("cella");
            if cella.exists() { Some(cella) } else { None }
        })
        .unwrap_or_else(|| std::path::PathBuf::from("cella"));

    info!(
        "Handling BranchRequest: branch={branch} base={base:?} via {}",
        cella_bin.display()
    );

    // Send initial progress
    send_message(
        writer,
        &DaemonMessage::OperationProgress {
            request_id: request_id.to_string(),
            step: "starting".to_string(),
            message: format!("Creating worktree branch '{branch}'..."),
        },
    )
    .await?;

    // Build command
    let mut cmd = Command::new(&cella_bin);
    cmd.arg("branch").arg(branch).arg("--output").arg("json");
    if let Some(b) = base {
        cmd.arg("--base").arg(b);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Spawn and wait
    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            let result = DaemonMessage::BranchResult {
                request_id: request_id.to_string(),
                result: WorktreeOperationResult::Error {
                    message: format!("failed to spawn cella branch: {e}"),
                },
            };
            send_message(writer, &result).await?;
            return Ok(());
        }
    };

    // Stream captured output
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: OutputStream::Stdout,
                data: stdout.to_string(),
            },
        )
        .await?;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: OutputStream::Stderr,
                data: stderr.to_string(),
            },
        )
        .await?;
    }

    // Send final result
    let result = if output.status.success() {
        // Try to parse JSON output from `cella branch --output json`
        // for container_name and worktree_path. Fall back to generic success.
        parse_branch_json_output(&stdout).unwrap_or_else(|| WorktreeOperationResult::Success {
            container_name: String::new(),
            worktree_path: String::new(),
        })
    } else {
        WorktreeOperationResult::Error {
            message: if stderr.is_empty() {
                format!("cella branch exited with code {}", output.status)
            } else {
                stderr.trim().to_string()
            },
        }
    };

    send_message(
        writer,
        &DaemonMessage::BranchResult {
            request_id: request_id.to_string(),
            result,
        },
    )
    .await?;

    Ok(())
}

/// Try to parse the JSON output from `cella branch --output json`.
fn parse_branch_json_output(stdout: &str) -> Option<cella_port::protocol::WorktreeOperationResult> {
    // The JSON output may contain multiple lines; find the last JSON object
    // which should be the final result.
    for line in stdout.lines().rev() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            let container_name = v
                .get("containerId")
                .or_else(|| v.get("containerName"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let worktree_path = v
                .get("workspaceFolder")
                .or_else(|| v.get("remoteWorkspaceFolder"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if v.get("outcome").is_some() || v.get("containerId").is_some() {
                return Some(cella_port::protocol::WorktreeOperationResult::Success {
                    container_name,
                    worktree_path,
                });
            }
        }
    }
    None
}

/// Handle a `ListRequest` using direct git + container queries.
async fn handle_list_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    // Find git repositories from registered containers.
    // For now, run `git worktree list --porcelain` in the CWD or a known repo.
    // This is a simplification — the proper version would discover the repo
    // from the requesting container's workspace_path label.
    let worktrees = match list_worktrees_from_cwd() {
        Ok(wts) => wts,
        Err(e) => {
            warn!("Failed to list worktrees: {e}");
            vec![]
        }
    };

    send_message(
        writer,
        &DaemonMessage::ListResult {
            request_id: request_id.to_string(),
            worktrees,
        },
    )
    .await?;

    Ok(())
}

/// List worktrees by running `git worktree list --porcelain` from CWD.
fn list_worktrees_from_cwd() -> Result<Vec<cella_port::protocol::WorktreeEntry>, String> {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut is_first = true;
    let mut is_bare = false;

    for line in stdout.lines() {
        if line.is_empty() {
            if let Some(p) = path.take() {
                entries.push(cella_port::protocol::WorktreeEntry {
                    branch: branch.take(),
                    worktree_path: p,
                    is_main: is_first && !is_bare,
                    container_name: None,
                    container_state: None,
                });
                is_first = false;
                is_bare = false;
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            let short = b.strip_prefix("refs/heads/").unwrap_or(b);
            branch = Some(short.to_string());
        } else if line == "bare" {
            is_bare = true;
        }
    }
    // Handle last entry without trailing blank line
    if let Some(p) = path {
        entries.push(cella_port::protocol::WorktreeEntry {
            branch: branch.take(),
            worktree_path: p,
            is_main: is_first && !is_bare,
            container_name: None,
            container_state: None,
        });
    }

    Ok(entries)
}

/// Extract the port number from a URL like `http://localhost:3000/path`.
fn extract_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://")?.1;
    let host_port = rest.find('/').map_or(rest, |i| &rest[..i]);
    host_port.rsplit_once(':')?.1.parse().ok()
}

/// Wait for a TCP proxy to accept connections, polling up to 2 seconds.
async fn wait_for_proxy_ready(port: u16) {
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    debug!("Proxy readiness timeout for port {port}, opening browser anyway");
}

/// Rewrite a browser URL to use the correct host port.
///
/// If the URL targets a container port that has been forwarded to a different
/// host port, this rewrites the URL to use the host port so it's reachable
/// from the host machine.
async fn rewrite_browser_url(
    url: &str,
    port_manager: &Arc<Mutex<PortManager>>,
    container_id: &str,
) -> String {
    // Try to parse as a URL with a port
    let Some((before_host, rest)) = url.split_once("://") else {
        return url.to_string();
    };

    // Extract host:port from the URL
    let (host_port_part, path) = rest
        .find('/')
        .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));

    let Some((host, port_str)) = host_port_part.rsplit_once(':') else {
        return url.to_string();
    };

    let Ok(container_port) = port_str.parse::<u16>() else {
        return url.to_string();
    };

    // Only rewrite localhost/127.0.0.1 URLs
    if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
        return url.to_string();
    }

    // Look up the forwarded host port
    let forwarded = {
        let pm = port_manager.lock().await;
        pm.all_forwarded_ports()
    };
    let mapping = forwarded
        .iter()
        .find(|p| p.container_id == container_id && p.container_port == container_port);

    if let Some(info) = mapping.filter(|info| info.host_port != container_port) {
        return format!("{before_host}://localhost:{}{path}", info.host_port);
    }

    url.to_string()
}

pub use crate::shared::current_time_secs;

#[cfg(test)]
mod tests {
    use cella_port::protocol::PortProtocol;

    use super::*;

    /// Helper to set up a port manager with a forwarded port for testing.
    async fn pm_with_forwarded_port(container_port: u16) -> Arc<Mutex<PortManager>> {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "test", Some("172.20.0.5".to_string()), vec![], None);
            guard.handle_port_open("c1", container_port, PortProtocol::Tcp, None);
        }
        pm
    }

    #[tokio::test]
    async fn rewrite_url_no_remap() {
        let pm = pm_with_forwarded_port(3000).await;
        // Port 3000 maps to 3000 (no conflict) — no rewrite
        let result = rewrite_browser_url("http://localhost:3000/callback", &pm, "c1").await;
        assert_eq!(result, "http://localhost:3000/callback");
    }

    #[tokio::test]
    async fn rewrite_url_with_remap() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "test-a", Some("172.20.0.5".to_string()), vec![], None);
            guard.register_container("c2", "test-b", Some("172.20.0.6".to_string()), vec![], None);
            // c1 gets port 3000
            guard.handle_port_open("c1", 3000, PortProtocol::Tcp, None);
            // c2 also wants 3000 — gets remapped to 3001
            guard.handle_port_open("c2", 3000, PortProtocol::Tcp, None);
        }
        let result = rewrite_browser_url("http://localhost:3000/auth", &pm, "c2").await;
        assert_eq!(result, "http://localhost:3001/auth");
    }

    #[tokio::test]
    async fn rewrite_url_127_0_0_1() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "a", Some("172.20.0.5".to_string()), vec![], None);
            guard.register_container("c2", "b", Some("172.20.0.6".to_string()), vec![], None);
            guard.handle_port_open("c1", 8080, PortProtocol::Tcp, None);
            guard.handle_port_open("c2", 8080, PortProtocol::Tcp, None);
        }
        let result = rewrite_browser_url("http://127.0.0.1:8080", &pm, "c2").await;
        assert_eq!(result, "http://localhost:8081");
    }

    #[tokio::test]
    async fn rewrite_url_non_localhost_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("http://example.com:3000/path", &pm, "c1").await;
        assert_eq!(result, "http://example.com:3000/path");
    }

    #[tokio::test]
    async fn rewrite_url_no_port_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("https://github.com/login", &pm, "c1").await;
        assert_eq!(result, "https://github.com/login");
    }

    #[tokio::test]
    async fn rewrite_url_unknown_port_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("http://localhost:9999/path", &pm, "c1").await;
        assert_eq!(result, "http://localhost:9999/path");
    }
}
