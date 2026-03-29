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
    pub task_manager: crate::task_manager::SharedTaskManager,
    /// Host-native cella binary, resolved and snapshotted at daemon startup so
    /// that in-container `cargo build` cannot clobber it via the bind mount.
    pub cella_bin: std::path::PathBuf,
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
    /// Host-side workspace path from container labels.
    workspace_path: Option<String>,
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

    // Send DaemonHello success with workspace metadata from container labels.
    let labels = lookup_container_labels(&container_id).await;
    let hello = DaemonHello {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        error: None,
        workspace_path: labels.get("dev.cella.workspace_path").cloned(),
        parent_repo: labels.get("dev.cella.parent_repo").cloned(),
        is_worktree: labels
            .get("dev.cella.worktree")
            .is_some_and(|v| v == "true"),
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

    let workspace_path = hello.workspace_path.clone();

    Ok(Some(HandshakeResult {
        container_name,
        container_id,
        container_ip,
        agent_state,
        workspace_path,
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

        // Worktree/exec/task operations need writer access for multi-message responses.
        // Handle them directly before falling through to the single-response handler.
        if matches!(
            &msg,
            AgentMessage::BranchRequest { .. }
                | AgentMessage::ListRequest { .. }
                | AgentMessage::ExecRequest { .. }
                | AgentMessage::PruneRequest { .. }
                | AgentMessage::TaskRunRequest { .. }
                | AgentMessage::TaskListRequest { .. }
                | AgentMessage::TaskLogsRequest { .. }
                | AgentMessage::TaskWaitRequest { .. }
                | AgentMessage::TaskStopRequest { .. }
                | AgentMessage::SwitchRequest { .. }
        ) {
            handle_worktree_message(
                msg,
                WorktreeHandlerCtx {
                    workspace_path: hs.workspace_path.as_deref(),
                    cella_bin: &ctx.cella_bin,
                    task_mgr: &ctx.task_manager,
                },
                &mut writer,
            )
            .await?;
            continue;
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

        // Worktree/exec/task operations are handled in the message loop via
        // handle_worktree_message() which has writer access for streaming.
        AgentMessage::BranchRequest { .. }
        | AgentMessage::ListRequest { .. }
        | AgentMessage::ExecRequest { .. }
        | AgentMessage::PruneRequest { .. }
        | AgentMessage::TaskRunRequest { .. }
        | AgentMessage::TaskListRequest { .. }
        | AgentMessage::TaskLogsRequest { .. }
        | AgentMessage::TaskWaitRequest { .. }
        | AgentMessage::TaskStopRequest { .. }
        | AgentMessage::SwitchRequest { .. } => {
            unreachable!("worktree/exec/task messages intercepted before handle_agent_message")
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
// Docker label lookup
// ---------------------------------------------------------------------------

/// Look up container labels via `docker inspect`.
///
/// Returns an empty map on any failure (Docker not running, container not found, etc.)
/// so callers can proceed with defaults.
async fn lookup_container_labels(container_id: &str) -> HashMap<String, String> {
    let output = tokio::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{json .Config.Labels}}",
            container_id,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<HashMap<String, String>>(stdout.trim()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Worktree operation handlers
// ---------------------------------------------------------------------------

/// Shared context for worktree operation handlers.
struct WorktreeHandlerCtx<'a> {
    workspace_path: Option<&'a str>,
    cella_bin: &'a std::path::Path,
    task_mgr: &'a crate::task_manager::SharedTaskManager,
}

/// Handle worktree/exec/task messages that need writer access for multi-message responses.
async fn handle_worktree_message<W: AsyncWriteExt + Unpin>(
    msg: AgentMessage,
    wt: WorktreeHandlerCtx<'_>,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    match msg {
        AgentMessage::BranchRequest {
            request_id,
            branch,
            base,
        } => {
            handle_branch_request(
                &request_id,
                &branch,
                base.as_deref(),
                wt.workspace_path,
                wt.cella_bin,
                writer,
            )
            .await?;
        }
        AgentMessage::ListRequest { request_id } => {
            handle_list_request(&request_id, wt.workspace_path, writer).await?;
        }
        AgentMessage::ExecRequest {
            request_id,
            branch,
            command,
        } => {
            handle_exec_request(&request_id, &branch, &command, writer).await?;
        }
        AgentMessage::PruneRequest {
            request_id,
            dry_run,
        } => {
            handle_prune_request(&request_id, dry_run, wt.workspace_path, wt.cella_bin, writer)
                .await?;
        }
        AgentMessage::TaskRunRequest {
            request_id,
            branch,
            command,
            base,
        } => {
            handle_task_run(&request_id, &branch, &command, base.as_deref(), &wt, writer)
                .await?;
        }
        AgentMessage::TaskListRequest { request_id } => {
            handle_task_list(&request_id, wt.task_mgr, writer).await?;
        }
        AgentMessage::TaskLogsRequest { request_id, branch } => {
            handle_task_logs(&request_id, &branch, wt.task_mgr, writer).await?;
        }
        AgentMessage::TaskWaitRequest { request_id, branch } => {
            handle_task_wait(&request_id, &branch, wt.task_mgr, writer).await?;
        }
        AgentMessage::TaskStopRequest { request_id, branch } => {
            handle_task_stop(&request_id, &branch, wt.task_mgr, writer).await?;
        }
        AgentMessage::SwitchRequest { request_id, branch } => {
            handle_switch_request(&request_id, &branch, writer).await?;
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
    workspace_path: Option<&str>,
    cella_bin: &std::path::Path,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    use cella_port::protocol::WorktreeOperationResult;
    use tokio::process::Command;

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
    let mut cmd = Command::new(cella_bin);
    cmd.arg("branch").arg(branch).arg("--output").arg("json");
    if let Some(b) = base {
        cmd.arg("--base").arg(b);
    }
    if let Some(ws) = workspace_path {
        cmd.current_dir(ws);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Spawn child process with piped stdout/stderr for live streaming.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_message(
                writer,
                &DaemonMessage::BranchResult {
                    request_id: request_id.to_string(),
                    result: WorktreeOperationResult::Error {
                        message: format!("failed to spawn cella branch: {e}"),
                    },
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (status, last_stdout_line, stderr_collected) =
        stream_child_output(&mut child, request_id, writer).await?;

    // Send final result.
    let result = if status.success() {
        parse_branch_json_output(&last_stdout_line).unwrap_or_else(|| {
            WorktreeOperationResult::Success {
                container_name: String::new(),
                worktree_path: String::new(),
            }
        })
    } else {
        WorktreeOperationResult::Error {
            message: if stderr_collected.is_empty() {
                format!("cella branch exited with code {status}")
            } else {
                stderr_collected.trim().to_string()
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

/// Stream a child process's stdout and stderr line-by-line as `OperationOutput` messages.
///
/// Returns the exit status, last stdout line (for JSON parsing), and collected stderr.
async fn stream_child_output<W: AsyncWriteExt + Unpin>(
    child: &mut tokio::process::Child,
    request_id: &str,
    writer: &mut W,
) -> Result<(std::process::ExitStatus, String, String), CellaDaemonError> {
    use cella_port::protocol::OutputStream;

    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();
    let mut last_stdout_line = String::new();

    let mut stdout_reader = child_stdout.map(|s| BufReader::new(s).lines());
    let mut stderr_reader = child_stderr.map(|s| BufReader::new(s).lines());
    let mut stdout_done = stdout_reader.is_none();
    let mut stderr_done = stderr_reader.is_none();
    let mut stderr_collected = String::new();

    while !stdout_done || !stderr_done {
        tokio::select! {
            line = async {
                if let Some(ref mut r) = stdout_reader { r.next_line().await } else { Ok(None) }
            }, if !stdout_done => {
                match line {
                    Ok(Some(text)) => {
                        last_stdout_line.clone_from(&text);
                        send_message(writer, &DaemonMessage::OperationOutput {
                            request_id: request_id.to_string(),
                            stream: OutputStream::Stdout,
                            data: format!("{text}\n"),
                        }).await?;
                    }
                    _ => stdout_done = true,
                }
            }
            line = async {
                if let Some(ref mut r) = stderr_reader { r.next_line().await } else { Ok(None) }
            }, if !stderr_done => {
                match line {
                    Ok(Some(text)) => {
                        stderr_collected.push_str(&text);
                        stderr_collected.push('\n');
                        send_message(writer, &DaemonMessage::OperationOutput {
                            request_id: request_id.to_string(),
                            stream: OutputStream::Stderr,
                            data: format!("{text}\n"),
                        }).await?;
                    }
                    _ => stderr_done = true,
                }
            }
        }
    }

    let status = child.wait().await.map_err(|e| CellaDaemonError::Protocol {
        message: format!("failed to wait for child process: {e}"),
    })?;

    Ok((status, last_stdout_line, stderr_collected))
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
    workspace_path: Option<&str>,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let mut worktrees = match list_worktrees(workspace_path) {
        Ok(wts) => wts,
        Err(e) => {
            warn!("Failed to list worktrees: {e}");
            vec![]
        }
    };

    // Enrich with container status from Docker
    let containers = list_cella_containers().await;
    for wt in &mut worktrees {
        if let Some(c) = containers
            .iter()
            .find(|c| c.workspace_path.as_deref() == Some(&*wt.worktree_path))
        {
            wt.container_name = Some(c.name.clone());
            wt.container_state = Some(c.state.clone());
        }
    }

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

/// List worktrees by running `git worktree list --porcelain`.
fn list_worktrees(
    workspace_path: Option<&str>,
) -> Result<Vec<cella_port::protocol::WorktreeEntry>, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["worktree", "list", "--porcelain"]);
    if let Some(ws) = workspace_path {
        cmd.current_dir(ws);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_worktree_porcelain(&stdout))
}

/// Parse `git worktree list --porcelain` output.
fn parse_worktree_porcelain(output: &str) -> Vec<cella_port::protocol::WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut is_first = true;
    let mut is_bare = false;

    for line in output.lines() {
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

    entries
}

/// Minimal container info from `docker ps`.
struct CellaContainer {
    name: String,
    state: String,
    workspace_path: Option<String>,
}

/// List all cella-managed containers with their workspace paths.
async fn list_cella_containers() -> Vec<CellaContainer> {
    // Use docker ps with label filter and Go template for structured output
    let output = tokio::process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=dev.cella.tool=cella",
            "--format",
            "{{.Names}}\t{{.State}}\t{{.Label \"dev.cella.workspace_path\"}}",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    let Ok(output) = output else {
        return vec![];
    };
    if !output.status.success() {
        return vec![];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let name = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let ws = parts
                .next()
                .map(ToString::to_string)
                .filter(|s| !s.is_empty());
            Some(CellaContainer {
                name,
                state,
                workspace_path: ws,
            })
        })
        .collect()
}

/// Handle an `ExecRequest` by finding the branch's container and running docker exec.
async fn handle_exec_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    command: &[String],
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    use cella_port::protocol::OutputStream;

    // Find the container for this branch via docker ps with label filter.
    let container_name = find_container_for_branch(branch).await;
    let Some(container_name) = container_name else {
        send_message(
            writer,
            &DaemonMessage::ExecResult {
                request_id: request_id.to_string(),
                exit_code: 1,
            },
        )
        .await?;
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: OutputStream::Stderr,
                data: format!("No running container found for branch '{branch}'\n"),
            },
        )
        .await?;
        return Ok(());
    };

    info!("Handling ExecRequest: branch={branch} container={container_name}");

    // Build docker exec command.
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec").arg(&container_name);
    for arg in command {
        cmd.arg(arg);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: OutputStream::Stderr,
                    data: format!("failed to spawn docker exec: {e}\n"),
                },
            )
            .await?;
            send_message(
                writer,
                &DaemonMessage::ExecResult {
                    request_id: request_id.to_string(),
                    exit_code: 1,
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (status, _, _) = stream_child_output(&mut child, request_id, writer).await?;

    send_message(
        writer,
        &DaemonMessage::ExecResult {
            request_id: request_id.to_string(),
            exit_code: status.code().unwrap_or(1),
        },
    )
    .await?;

    Ok(())
}

/// Handle a `PruneRequest` by spawning `cella prune` as a subprocess.
async fn handle_prune_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    dry_run: bool,
    workspace_path: Option<&str>,
    cella_bin: &std::path::Path,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {

    let mut cmd = tokio::process::Command::new(cella_bin);
    cmd.arg("prune").arg("--force"); // Skip interactive prompt
    if dry_run {
        cmd.arg("--dry-run");
    }
    if let Some(ws) = workspace_path {
        cmd.current_dir(ws);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_message(
                writer,
                &DaemonMessage::PruneResult {
                    request_id: request_id.to_string(),
                    pruned: vec![],
                    errors: vec![format!("failed to spawn cella prune: {e}")],
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (status, _stdout, stderr) = stream_child_output(&mut child, request_id, writer).await?;

    let errors = if status.success() {
        vec![]
    } else {
        vec![stderr.trim().to_string()]
    };

    send_message(
        writer,
        &DaemonMessage::PruneResult {
            request_id: request_id.to_string(),
            pruned: vec![], // TODO: parse pruned branches from output
            errors,
        },
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Task command handlers
// ---------------------------------------------------------------------------

/// Handle `TaskRunRequest`: create branch (if needed) + run background command.
async fn handle_task_run<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    command: &[String],
    base: Option<&str>,
    wt: &WorktreeHandlerCtx<'_>,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    // First, ensure the branch container exists (create if needed).
    // Check if container already exists for this branch.
    let container = find_container_for_branch(branch).await;
    let container_name = if let Some(name) = container {
        name
    } else {
        // Create branch + container via cella branch subprocess.
        send_message(
            writer,
            &DaemonMessage::OperationProgress {
                request_id: request_id.to_string(),
                step: "creating_branch".to_string(),
                message: format!("Creating branch '{branch}' and container..."),
            },
        )
        .await?;

        let mut cmd = tokio::process::Command::new(wt.cella_bin);
        cmd.arg("branch").arg(branch).arg("--output").arg("json");
        if let Some(b) = base {
            cmd.arg("--base").arg(b);
        }
        if let Some(ws) = wt.workspace_path {
            cmd.current_dir(ws);
        }
        let output = cmd.output().await.map_err(|e| CellaDaemonError::Protocol {
            message: format!("failed to create branch: {e}"),
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: cella_port::protocol::OutputStream::Stderr,
                    data: stderr.to_string(),
                },
            )
            .await?;
            send_message(
                writer,
                &DaemonMessage::TaskRunResult {
                    request_id: request_id.to_string(),
                    task_id: String::new(),
                    container_name: String::new(),
                },
            )
            .await?;
            return Ok(());
        }

        // Find the newly created container.
        find_container_for_branch(branch).await.unwrap_or_default()
    };

    if container_name.is_empty() {
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: cella_port::protocol::OutputStream::Stderr,
                data: format!("No container found for branch '{branch}' after creation\n"),
            },
        )
        .await?;
        return Ok(());
    }

    // Start the background task.
    let task_id = {
        let mut mgr = wt.task_mgr.lock().await;
        match mgr.start_task(branch, container_name.clone(), command.to_vec()) {
            Ok(id) => id,
            Err(e) => {
                send_message(
                    writer,
                    &DaemonMessage::OperationOutput {
                        request_id: request_id.to_string(),
                        stream: cella_port::protocol::OutputStream::Stderr,
                        data: format!("{e}\n"),
                    },
                )
                .await?;
                return Ok(());
            }
        }
    };

    send_message(
        writer,
        &DaemonMessage::TaskRunResult {
            request_id: request_id.to_string(),
            task_id,
            container_name,
        },
    )
    .await?;

    Ok(())
}

/// Handle `TaskListRequest`: list active tasks.
async fn handle_task_list<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    task_mgr: &crate::task_manager::SharedTaskManager,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let tasks = {
        let infos = task_mgr.lock().await.list_tasks().await;
        infos
            .into_iter()
            .map(|t| cella_port::protocol::TaskEntry {
                task_id: t.task_id,
                branch: t.branch,
                container_name: t.container_name,
                status: if t.is_done {
                    if t.exit_code == Some(0) {
                        cella_port::protocol::TaskStatus::Done
                    } else {
                        cella_port::protocol::TaskStatus::Failed
                    }
                } else {
                    cella_port::protocol::TaskStatus::Running
                },
                command: t.command,
                elapsed_secs: t.elapsed_secs,
            })
            .collect()
    };

    send_message(
        writer,
        &DaemonMessage::TaskListResult {
            request_id: request_id.to_string(),
            tasks,
        },
    )
    .await?;

    Ok(())
}

/// Handle `TaskLogsRequest`: return captured output for a task.
async fn handle_task_logs<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    task_mgr: &crate::task_manager::SharedTaskManager,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let output = task_mgr
        .lock()
        .await
        .get_output(branch)
        .await
        .unwrap_or_default();
    let is_done = task_mgr
        .lock()
        .await
        .list_tasks()
        .await
        .iter()
        .any(|t| t.branch == branch && t.is_done);

    send_message(
        writer,
        &DaemonMessage::TaskLogsData {
            request_id: request_id.to_string(),
            data: output,
            done: is_done,
        },
    )
    .await?;

    Ok(())
}

/// Handle `TaskWaitRequest`: block until task completes.
async fn handle_task_wait<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    task_mgr: &crate::task_manager::SharedTaskManager,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    // We need to drop the lock while waiting, so clone what we need.
    let exit_code = {
        let mgr = task_mgr.lock().await;
        mgr.wait_for(branch).await
    };

    send_message(
        writer,
        &DaemonMessage::TaskWaitResult {
            request_id: request_id.to_string(),
            exit_code: exit_code.unwrap_or(1),
        },
    )
    .await?;

    Ok(())
}

/// Handle `TaskStopRequest`: abort a running task.
async fn handle_task_stop<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    task_mgr: &crate::task_manager::SharedTaskManager,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let stopped = {
        let mut mgr = task_mgr.lock().await;
        mgr.stop_task(branch).await
    };

    if !stopped {
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: cella_port::protocol::OutputStream::Stderr,
                data: format!("No running task found for branch '{branch}'\n"),
            },
        )
        .await?;
    }

    send_message(
        writer,
        &DaemonMessage::TaskStopResult {
            request_id: request_id.to_string(),
        },
    )
    .await?;

    Ok(())
}

/// Handle a `SwitchRequest`: run default shell in the target branch's container.
///
/// This is a non-interactive exec — suitable for AI agents. For interactive
/// use, users should run `cella shell <branch>` from the host terminal.
async fn handle_switch_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let container_name = find_container_for_branch(branch).await;
    let Some(container_name) = container_name else {
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: cella_port::protocol::OutputStream::Stderr,
                data: format!("No running container found for branch '{branch}'\n"),
            },
        )
        .await?;
        send_message(
            writer,
            &DaemonMessage::SwitchResult {
                request_id: request_id.to_string(),
                exit_code: 1,
            },
        )
        .await?;
        return Ok(());
    };

    info!("Handling SwitchRequest: branch={branch} container={container_name}");

    // Run default shell in the target container.
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args([
        "exec",
        &container_name,
        "sh",
        "-c",
        "echo \"Connected to ${HOSTNAME:-$container_name}\"; exec $SHELL -l 2>/dev/null || exec sh",
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: cella_port::protocol::OutputStream::Stderr,
                    data: format!("Failed to exec shell: {e}\n"),
                },
            )
            .await?;
            send_message(
                writer,
                &DaemonMessage::SwitchResult {
                    request_id: request_id.to_string(),
                    exit_code: 1,
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (status, _, _) = stream_child_output(&mut child, request_id, writer).await?;

    send_message(
        writer,
        &DaemonMessage::SwitchResult {
            request_id: request_id.to_string(),
            exit_code: status.code().unwrap_or(1),
        },
    )
    .await?;

    Ok(())
}

/// Find the cella binary path (sibling to current exe or PATH fallback).
/// Resolve the host-native cella binary and snapshot it to a stable path.
///
/// During development, `cargo build` inside the container overwrites
/// `target/debug/cella` (the host macOS binary) with a Linux ELF via the
/// bind mount. By copying the binary to a daemon-managed location at startup,
/// we guarantee the daemon always has a working host-native binary.
///
/// Resolution order:
/// 1. `CELLA_HOST_BIN` env var (explicit override for development)
/// 2. Adjacent to the daemon exe (`target/debug/cella` next to `cella-daemon`)
/// 3. PATH lookup (installed `cella`)
pub(crate) fn resolve_cella_binary() -> std::path::PathBuf {
    use std::path::PathBuf;

    // 1. Explicit override
    if let Ok(bin) = std::env::var("CELLA_HOST_BIN") {
        let p = PathBuf::from(&bin);
        if p.is_file() {
            return snapshot_binary(&p).unwrap_or(p);
        }
        warn!("CELLA_HOST_BIN={bin} does not exist, falling through");
    }

    // 2. Adjacent to daemon exe
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let cella = dir.join("cella");
        if cella.is_file() {
            return snapshot_binary(&cella).unwrap_or(cella);
        }
    }

    // 3. PATH lookup
    if let Ok(output) = std::process::Command::new("which")
        .arg("cella")
        .output()
        && output.status.success()
    {
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if path.is_file() {
            return snapshot_binary(&path).unwrap_or(path);
        }
    }

    warn!("Could not resolve host cella binary — worktree operations will fail");
    PathBuf::from("cella")
}

/// Copy the binary to a daemon-managed stable path so bind-mount overwrites
/// from in-container builds don't affect the running daemon.
fn snapshot_binary(source: &std::path::Path) -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let snap_dir = std::path::PathBuf::from(home).join(".cella").join("bin");

    if std::fs::create_dir_all(&snap_dir).is_err() {
        return None;
    }

    let dest = snap_dir.join("cella");

    // Only copy if source is newer or snapshot doesn't exist yet.
    let needs_copy = match (std::fs::metadata(source), std::fs::metadata(&dest)) {
        (Ok(src_meta), Ok(dst_meta)) => {
            src_meta.modified().ok() > dst_meta.modified().ok()
                || src_meta.len() != dst_meta.len()
        }
        (Ok(_), Err(_)) => true,
        _ => return None,
    };

    if needs_copy {
        if let Err(e) = std::fs::copy(source, &dest) {
            warn!("Failed to snapshot cella binary: {e}");
            return None;
        }
        // Ensure executable permission
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
        }
        info!("Snapshotted cella binary to {}", dest.display());
    }

    Some(dest)
}

/// Find a running container for a given branch name via Docker labels.
async fn find_container_for_branch(branch: &str) -> Option<String> {
    let output = tokio::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("label=dev.cella.branch={branch}"),
            "--filter",
            "label=dev.cella.worktree=true",
            "--format",
            "{{.Names}}",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(ToString::to_string)
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
