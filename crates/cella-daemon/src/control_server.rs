//! TCP control server: receives messages from in-container agents.
//!
//! A single TCP listener (bound at daemon startup) accepts connections from all
//! containers.  Each agent identifies itself via `AgentHello.container_name` and
//! is validated against the daemon's auth token.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cella_protocol::{
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
                | AgentMessage::DownRequest { .. }
                | AgentMessage::UpRequest { .. }
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
        | AgentMessage::DownRequest { .. }
        | AgentMessage::UpRequest { .. }
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
            all,
        } => {
            handle_prune_request(
                &request_id,
                dry_run,
                all,
                wt.workspace_path,
                wt.cella_bin,
                writer,
            )
            .await?;
        }
        AgentMessage::DownRequest {
            request_id,
            branch,
            rm,
            volumes,
            force,
        } => {
            handle_down_request(&request_id, &branch, rm, volumes, force, &wt, writer).await?;
        }
        AgentMessage::UpRequest {
            request_id,
            branch,
            rebuild,
        } => {
            handle_up_request(
                &request_id,
                &branch,
                rebuild,
                wt.workspace_path,
                wt.cella_bin,
                writer,
            )
            .await?;
        }
        AgentMessage::TaskRunRequest {
            request_id,
            branch,
            command,
            base,
        } => {
            handle_task_run(&request_id, &branch, &command, base.as_deref(), &wt, writer).await?;
        }
        AgentMessage::TaskListRequest { request_id } => {
            handle_task_list(&request_id, wt.task_mgr, writer).await?;
        }
        AgentMessage::TaskLogsRequest {
            request_id,
            branch,
            follow,
        } => {
            handle_task_logs(&request_id, &branch, follow, wt.task_mgr, writer).await?;
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
    use cella_protocol::WorktreeOperationResult;
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
            WorktreeOperationResult::Error {
                message: format!(
                    "operation may have succeeded but output was unparseable: {last_stdout_line}"
                ),
            }
        })
    } else {
        WorktreeOperationResult::Error {
            message: if stderr_collected.is_empty() {
                format!("cella branch exited with code {status}")
            } else {
                format!(
                    "cella branch failed (exit {status}): {}",
                    stderr_collected.trim()
                )
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
    use cella_protocol::OutputStream;

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
fn parse_branch_json_output(stdout: &str) -> Option<cella_protocol::WorktreeOperationResult> {
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
                return Some(cella_protocol::WorktreeOperationResult::Success {
                    container_name,
                    worktree_path,
                });
            }
        }
    }
    None
}

/// Parse JSON output from `cella prune --output json`.
///
/// Expected format: `{"pruned":["branch1","branch2"],"errors":["msg"]}`
fn parse_prune_json_output(stdout: &str) -> (Vec<String>, Vec<String>) {
    fn extract_strings(v: &serde_json::Value, key: &str) -> Vec<String> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    let trimmed = stdout.trim();
    serde_json::from_str::<serde_json::Value>(trimmed).map_or_else(
        |_| (vec![], vec![]),
        |v| (extract_strings(&v, "pruned"), extract_strings(&v, "errors")),
    )
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
) -> Result<Vec<cella_protocol::WorktreeEntry>, String> {
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
fn parse_worktree_porcelain(output: &str) -> Vec<cella_protocol::WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut is_first = true;
    let mut is_bare = false;

    for line in output.lines() {
        if line.is_empty() {
            if let Some(p) = path.take() {
                entries.push(cella_protocol::WorktreeEntry {
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
        entries.push(cella_protocol::WorktreeEntry {
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
    use cella_protocol::OutputStream;

    // Find the container for this branch via docker ps with label filter.
    let container_name = find_container_for_branch(branch).await;
    let Some(container_name) = container_name else {
        send_message(
            writer,
            &DaemonMessage::OperationOutput {
                request_id: request_id.to_string(),
                stream: OutputStream::Stderr,
                data: format!("No running container found for branch '{branch}'\n"),
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
    all: bool,
    workspace_path: Option<&str>,
    cella_bin: &std::path::Path,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let mut cmd = tokio::process::Command::new(cella_bin);
    cmd.arg("prune").arg("--force").arg("--output").arg("json");
    if dry_run {
        cmd.arg("--dry-run");
    }
    if all {
        cmd.arg("--all");
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

    let (status, last_stdout, stderr) = stream_child_output(&mut child, request_id, writer).await?;

    // Parse JSON result from stdout.
    let (pruned, mut errors) = parse_prune_json_output(&last_stdout);
    if !status.success() && errors.is_empty() {
        errors.push(stderr.trim().to_string());
    }

    send_message(
        writer,
        &DaemonMessage::PruneResult {
            request_id: request_id.to_string(),
            pruned,
            errors,
        },
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Down/Up command handlers
// ---------------------------------------------------------------------------

/// Handle a `DownRequest` by spawning `cella down --branch` as a subprocess.
async fn handle_down_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    rm: bool,
    volumes: bool,
    force: bool,
    wt: &WorktreeHandlerCtx<'_>,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    info!("Handling DownRequest: branch={branch} rm={rm} volumes={volumes} force={force}");

    send_message(
        writer,
        &DaemonMessage::OperationProgress {
            request_id: request_id.to_string(),
            step: "stopping".to_string(),
            message: format!("Stopping container for branch '{branch}'..."),
        },
    )
    .await?;

    let mut cmd = tokio::process::Command::new(wt.cella_bin);
    cmd.arg("down")
        .arg("--branch")
        .arg(branch)
        .arg("--output")
        .arg("json");
    if rm {
        cmd.arg("--rm");
    }
    if volumes {
        cmd.arg("--volumes");
    }
    if force {
        cmd.arg("--force");
    }
    if let Some(ws) = wt.workspace_path {
        cmd.current_dir(ws);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let error_msg = format!("failed to spawn cella down: {e}");
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: cella_protocol::OutputStream::Stderr,
                    data: format!("{error_msg}\n"),
                },
            )
            .await?;
            send_message(
                writer,
                &DaemonMessage::DownResult {
                    request_id: request_id.to_string(),
                    result: cella_protocol::DownOperationResult::Error { message: error_msg },
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (status, last_stdout, stderr) = stream_child_output(&mut child, request_id, writer).await?;

    let result = if status.success() {
        parse_down_json_output(&last_stdout)
    } else {
        if !stderr.trim().is_empty() {
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: cella_protocol::OutputStream::Stderr,
                    data: stderr.clone(),
                },
            )
            .await?;
        }
        let error_msg = if stderr.trim().is_empty() {
            format!("cella down exited with {status}")
        } else {
            stderr.trim().to_string()
        };
        cella_protocol::DownOperationResult::Error { message: error_msg }
    };

    send_message(
        writer,
        &DaemonMessage::DownResult {
            request_id: request_id.to_string(),
            result,
        },
    )
    .await?;

    Ok(())
}

/// Parse JSON output from `cella down --output json`.
fn parse_down_json_output(stdout: &str) -> cella_protocol::DownOperationResult {
    use cella_protocol::{DownOperationResult, DownOutcome};

    let trimmed = stdout.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let outcome_str = v
                .get("outcome")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let outcome = match outcome_str {
                "removed" => DownOutcome::Removed,
                _ => DownOutcome::Stopped,
            };
            let container_name = v
                .get("containerId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            DownOperationResult::Success {
                outcome,
                container_name,
            }
        }
        Err(e) => DownOperationResult::Error {
            message: format!("operation may have succeeded but output was unparseable: {e}"),
        },
    }
}

/// Handle an `UpRequest` by spawning `cella up --branch` as a subprocess.
async fn handle_up_request<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    rebuild: bool,
    workspace_path: Option<&str>,
    cella_bin: &std::path::Path,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    use cella_protocol::WorktreeOperationResult;

    info!("Handling UpRequest: branch={branch} rebuild={rebuild}");

    send_message(
        writer,
        &DaemonMessage::OperationProgress {
            request_id: request_id.to_string(),
            step: "starting".to_string(),
            message: format!("Starting container for branch '{branch}'..."),
        },
    )
    .await?;

    let mut cmd = tokio::process::Command::new(cella_bin);
    cmd.arg("up")
        .arg("--branch")
        .arg(branch)
        .arg("--output")
        .arg("json");
    if rebuild {
        cmd.arg("--rebuild");
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
                &DaemonMessage::UpResult {
                    request_id: request_id.to_string(),
                    result: WorktreeOperationResult::Error {
                        message: format!("failed to spawn cella up: {e}"),
                    },
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (status, last_stdout, stderr) = stream_child_output(&mut child, request_id, writer).await?;

    let result = if status.success() {
        // Parse JSON output for container info.
        parse_up_json_output(&last_stdout)
    } else {
        let error_msg = if stderr.trim().is_empty() {
            format!("cella up exited with {status}")
        } else {
            stderr.trim().to_string()
        };
        WorktreeOperationResult::Error { message: error_msg }
    };

    send_message(
        writer,
        &DaemonMessage::UpResult {
            request_id: request_id.to_string(),
            result,
        },
    )
    .await?;

    Ok(())
}

/// Parse JSON output from `cella up --output json`.
fn parse_up_json_output(stdout: &str) -> cella_protocol::WorktreeOperationResult {
    use cella_protocol::WorktreeOperationResult;

    let trimmed = stdout.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let container_name = v
                .get("containerId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let worktree_path = v
                .get("workspaceFolder")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            WorktreeOperationResult::Success {
                container_name,
                worktree_path,
            }
        }
        Err(e) => WorktreeOperationResult::Error {
            message: format!("failed to parse up output: {e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Task command handlers
// ---------------------------------------------------------------------------

/// Handle `TaskRunRequest`: create branch (if needed) + run background command.
/// Ensure a branch has a running container, creating one if needed.
///
/// Returns the container name on success, or sends an error result and returns `None`.
async fn ensure_branch_container<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    base: Option<&str>,
    wt: &WorktreeHandlerCtx<'_>,
    writer: &mut W,
) -> Result<Option<String>, CellaDaemonError> {
    if let Some(name) = find_container_for_branch(branch).await {
        return Ok(Some(name));
    }

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
                stream: cella_protocol::OutputStream::Stderr,
                data: stderr.to_string(),
            },
        )
        .await?;
        return Ok(None);
    }

    Ok(find_container_for_branch(branch).await)
}

async fn handle_task_run<W: AsyncWriteExt + Unpin>(
    request_id: &str,
    branch: &str,
    command: &[String],
    base: Option<&str>,
    wt: &WorktreeHandlerCtx<'_>,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    let container_name = match ensure_branch_container(request_id, branch, base, wt, writer).await?
    {
        Some(name) if !name.is_empty() => name,
        _ => {
            let error_msg = format!("No container found for branch '{branch}' after creation");
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: cella_protocol::OutputStream::Stderr,
                    data: format!("{error_msg}\n"),
                },
            )
            .await?;
            send_message(
                writer,
                &DaemonMessage::TaskRunResult {
                    request_id: request_id.to_string(),
                    result: cella_protocol::TaskRunOperationResult::Error {
                        message: error_msg,
                    },
                },
            )
            .await?;
            return Ok(());
        }
    };

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
                        stream: cella_protocol::OutputStream::Stderr,
                        data: format!("{e}\n"),
                    },
                )
                .await?;
                send_message(
                    writer,
                    &DaemonMessage::TaskRunResult {
                        request_id: request_id.to_string(),
                        result: cella_protocol::TaskRunOperationResult::Error { message: e },
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
            result: cella_protocol::TaskRunOperationResult::Success {
                task_id,
                container_name,
            },
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
            .map(|t| cella_protocol::TaskEntry {
                task_id: t.task_id,
                branch: t.branch,
                container_name: t.container_name,
                status: if t.is_done {
                    if t.exit_code == Some(0) {
                        cella_protocol::TaskStatus::Done
                    } else {
                        cella_protocol::TaskStatus::Failed
                    }
                } else {
                    cella_protocol::TaskStatus::Running
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
    follow: bool,
    task_mgr: &crate::task_manager::SharedTaskManager,
    writer: &mut W,
) -> Result<(), CellaDaemonError> {
    if follow {
        // Send existing output snapshot first.
        let snapshot = task_mgr
            .lock()
            .await
            .get_output(branch)
            .await
            .unwrap_or_default();
        if !snapshot.is_empty() {
            send_message(
                writer,
                &DaemonMessage::TaskLogsData {
                    request_id: request_id.to_string(),
                    data: snapshot,
                    done: false,
                },
            )
            .await?;
        }

        // Subscribe and stream live output.
        let rx = task_mgr.lock().await.subscribe(branch);
        if let Some(mut rx) = rx {
            loop {
                match rx.recv().await {
                    Ok(chunk) => {
                        send_message(
                            writer,
                            &DaemonMessage::TaskLogsData {
                                request_id: request_id.to_string(),
                                data: chunk,
                                done: false,
                            },
                        )
                        .await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("task logs subscriber lagged by {n} messages");
                    }
                }
            }
        }

        send_message(
            writer,
            &DaemonMessage::TaskLogsData {
                request_id: request_id.to_string(),
                data: String::new(),
                done: true,
            },
        )
        .await?;
    } else {
        // Snapshot mode: dump available output and return.
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
    }

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
                stream: cella_protocol::OutputStream::Stderr,
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

/// Handle a `SwitchRequest`: open an interactive shell in the target container
/// via a PTY-backed TCP stream bridge.
///
/// The daemon allocates a PTY, spawns `docker exec -it` inside it, and opens
/// a TCP listener on a random port. It sends `StreamReady { port }` to the
/// agent, which connects and forwards its terminal stdin/stdout over the
/// raw TCP connection.
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
                stream: cella_protocol::OutputStream::Stderr,
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

    // Start PTY + TCP stream bridge.
    let session = match crate::stream_bridge::start_stream_bridge(&container_name, "0.0.0.0") {
        Ok(s) => s,
        Err(e) => {
            send_message(
                writer,
                &DaemonMessage::OperationOutput {
                    request_id: request_id.to_string(),
                    stream: cella_protocol::OutputStream::Stderr,
                    data: format!("Failed to start stream bridge: {e}\n"),
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

    // Tell the agent which port to connect to.
    send_message(
        writer,
        &DaemonMessage::StreamReady {
            request_id: request_id.to_string(),
            port: session.port,
        },
    )
    .await?;

    // Wait for the session to complete.
    let exit_code = session.handle.await.unwrap_or(1);

    send_message(
        writer,
        &DaemonMessage::SwitchResult {
            request_id: request_id.to_string(),
            exit_code,
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
    if let Ok(output) = std::process::Command::new("which").arg("cella").output()
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
            src_meta.modified().ok() > dst_meta.modified().ok() || src_meta.len() != dst_meta.len()
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
    use cella_protocol::PortProtocol;

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

    // -- AgentConnectionState --

    #[test]
    fn agent_state_new_defaults() {
        let state = AgentConnectionState::new();
        assert!(!state.connected.load(Ordering::Relaxed));
        assert_eq!(state.last_seen_secs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn agent_state_default_matches_new() {
        let state = AgentConnectionState::default();
        assert!(!state.connected.load(Ordering::Relaxed));
        assert_eq!(state.last_seen_secs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn agent_state_can_be_mutated() {
        let state = AgentConnectionState::new();
        state.connected.store(true, Ordering::Relaxed);
        state.last_seen_secs.store(1234, Ordering::Relaxed);
        assert!(state.connected.load(Ordering::Relaxed));
        assert_eq!(state.last_seen_secs.load(Ordering::Relaxed), 1234);
    }

    // -- ContainerHandle --

    #[test]
    fn container_handle_fields() {
        let handle = ContainerHandle {
            container_id: "abc123".into(),
            agent_state: Arc::new(AgentConnectionState::new()),
        };
        assert_eq!(handle.container_id, "abc123");
        assert!(!handle.agent_state.connected.load(Ordering::Relaxed));
    }

    // ---------------------------------------------------------------
    // extract_port
    // ---------------------------------------------------------------

    #[test]
    fn extract_port_standard_url() {
        assert_eq!(extract_port("http://localhost:3000/path"), Some(3000));
    }

    #[test]
    fn extract_port_https() {
        assert_eq!(extract_port("https://localhost:8443"), Some(8443));
    }

    #[test]
    fn extract_port_no_path() {
        assert_eq!(extract_port("http://localhost:8080"), Some(8080));
    }

    #[test]
    fn extract_port_with_trailing_slash() {
        assert_eq!(extract_port("http://localhost:5000/"), Some(5000));
    }

    #[test]
    fn extract_port_no_port_in_url() {
        assert_eq!(extract_port("http://localhost/path"), None);
    }

    #[test]
    fn extract_port_no_scheme() {
        assert_eq!(extract_port("localhost:3000"), None);
    }

    #[test]
    fn extract_port_empty_string() {
        assert_eq!(extract_port(""), None);
    }

    #[test]
    fn extract_port_ip_address() {
        assert_eq!(extract_port("http://127.0.0.1:9000/api"), Some(9000));
    }

    #[test]
    fn extract_port_invalid_port_number() {
        assert_eq!(extract_port("http://localhost:notaport/path"), None);
    }

    #[test]
    fn extract_port_port_zero() {
        assert_eq!(extract_port("http://localhost:0/path"), Some(0));
    }

    #[test]
    fn extract_port_max_port() {
        assert_eq!(extract_port("http://localhost:65535/path"), Some(65535));
    }

    #[test]
    fn extract_port_overflow_port() {
        // 65536 does not fit in u16
        assert_eq!(extract_port("http://localhost:65536/path"), None);
    }

    // ---------------------------------------------------------------
    // parse_worktree_porcelain
    // ---------------------------------------------------------------

    #[test]
    fn parse_worktree_porcelain_empty() {
        let entries = parse_worktree_porcelain("");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_worktree_porcelain_single_main() {
        let output = "worktree /home/user/repo\nbranch refs/heads/main\n\n";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].worktree_path, "/home/user/repo");
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert!(entries[0].is_main);
        assert!(entries[0].container_name.is_none());
        assert!(entries[0].container_state.is_none());
    }

    #[test]
    fn parse_worktree_porcelain_strips_refs_heads() {
        let output = "worktree /repo\nbranch refs/heads/feature/my-branch\n\n";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries[0].branch.as_deref(), Some("feature/my-branch"));
    }

    #[test]
    fn parse_worktree_porcelain_bare_not_main() {
        let output = "worktree /repo.git\nbare\n\n";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_main); // bare repos are not main
    }

    #[test]
    fn parse_worktree_porcelain_multiple() {
        let output = "\
worktree /home/user/repo
branch refs/heads/main

worktree /home/user/repo-feat
branch refs/heads/feat/x

";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].worktree_path, "/home/user/repo");
        assert!(entries[0].is_main);
        assert_eq!(entries[1].worktree_path, "/home/user/repo-feat");
        assert_eq!(entries[1].branch.as_deref(), Some("feat/x"));
        assert!(!entries[1].is_main);
    }

    #[test]
    fn parse_worktree_porcelain_no_branch() {
        // Detached HEAD — no branch line
        let output = "worktree /repo\nHEAD abc123\n\n";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].branch.is_none());
    }

    #[test]
    fn parse_worktree_porcelain_no_trailing_blank_line() {
        // Some git versions might not emit a trailing blank line
        let output = "worktree /repo\nbranch refs/heads/main";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].worktree_path, "/repo");
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_worktree_porcelain_three_worktrees() {
        let output = "\
worktree /main
branch refs/heads/main

worktree /feat-a
branch refs/heads/feat-a

worktree /feat-b
branch refs/heads/feat-b

";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_main);
        assert!(!entries[1].is_main);
        assert!(!entries[2].is_main);
    }

    #[test]
    fn parse_worktree_porcelain_branch_without_refs_heads() {
        // Rare but possible: branch that doesn't start with refs/heads/
        let output = "worktree /repo\nbranch refs/tags/v1.0\n\n";
        let entries = parse_worktree_porcelain(output);
        assert_eq!(entries[0].branch.as_deref(), Some("refs/tags/v1.0"));
    }

    // ---------------------------------------------------------------
    // parse_branch_json_output
    // ---------------------------------------------------------------

    #[test]
    fn parse_branch_json_with_container_id() {
        let json = r#"{"containerId":"my-container","workspaceFolder":"/ws"}"#;
        let result = parse_branch_json_output(json);
        assert!(result.is_some());
        if let Some(cella_protocol::WorktreeOperationResult::Success {
            container_name,
            worktree_path,
        }) = result
        {
            assert_eq!(container_name, "my-container");
            assert_eq!(worktree_path, "/ws");
        } else {
            panic!("Expected Success variant");
        }
    }

    #[test]
    fn parse_branch_json_with_outcome() {
        let json = r#"{"outcome":"created","containerId":"c1","workspaceFolder":"/ws"}"#;
        let result = parse_branch_json_output(json);
        assert!(result.is_some());
    }

    #[test]
    fn parse_branch_json_with_container_name_key() {
        let json = r#"{"containerName":"my-container","remoteWorkspaceFolder":"/remote/ws"}"#;
        let result = parse_branch_json_output(json);
        // containerName without containerId or outcome — should return None
        assert!(result.is_none());
    }

    #[test]
    fn parse_branch_json_invalid_json() {
        let result = parse_branch_json_output("not json at all");
        assert!(result.is_none());
    }

    #[test]
    fn parse_branch_json_empty_string() {
        let result = parse_branch_json_output("");
        assert!(result.is_none());
    }

    #[test]
    fn parse_branch_json_multiline_last_object() {
        let json = "some progress line\n{\"containerId\":\"c1\",\"workspaceFolder\":\"/ws\"}";
        let result = parse_branch_json_output(json);
        assert!(result.is_some());
    }

    #[test]
    fn parse_branch_json_no_matching_keys() {
        let json = r#"{"status":"ok","data":"value"}"#;
        let result = parse_branch_json_output(json);
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    // parse_prune_json_output
    // ---------------------------------------------------------------

    #[test]
    fn parse_prune_json_both_fields() {
        let json = r#"{"pruned":["branch-a","branch-b"],"errors":["err1"]}"#;
        let (pruned, errors) = parse_prune_json_output(json);
        assert_eq!(pruned, vec!["branch-a", "branch-b"]);
        assert_eq!(errors, vec!["err1"]);
    }

    #[test]
    fn parse_prune_json_empty_arrays() {
        let json = r#"{"pruned":[],"errors":[]}"#;
        let (pruned, errors) = parse_prune_json_output(json);
        assert!(pruned.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_prune_json_missing_fields() {
        let json = r"{}";
        let (pruned, errors) = parse_prune_json_output(json);
        assert!(pruned.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_prune_json_invalid() {
        let (pruned, errors) = parse_prune_json_output("not json");
        assert!(pruned.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_prune_json_empty() {
        let (pruned, errors) = parse_prune_json_output("");
        assert!(pruned.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_prune_json_only_pruned() {
        let json = r#"{"pruned":["x"]}"#;
        let (pruned, errors) = parse_prune_json_output(json);
        assert_eq!(pruned, vec!["x"]);
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_prune_json_only_errors() {
        let json = r#"{"errors":["failed"]}"#;
        let (pruned, errors) = parse_prune_json_output(json);
        assert!(pruned.is_empty());
        assert_eq!(errors, vec!["failed"]);
    }

    #[test]
    fn parse_prune_json_with_whitespace() {
        let json = "  {\"pruned\":[\"a\"]}  \n";
        let (pruned, _) = parse_prune_json_output(json);
        assert_eq!(pruned, vec!["a"]);
    }

    // ---------------------------------------------------------------
    // parse_down_json_output
    // ---------------------------------------------------------------

    #[test]
    fn parse_down_json_stopped() {
        let json = r#"{"outcome":"stopped","containerId":"my-container"}"#;
        let result = parse_down_json_output(json);
        match result {
            cella_protocol::DownOperationResult::Success {
                outcome,
                container_name,
            } => {
                assert!(matches!(
                    outcome,
                    cella_protocol::DownOutcome::Stopped
                ));
                assert_eq!(container_name, "my-container");
            }
            cella_protocol::DownOperationResult::Error { .. } => {
                panic!("Expected Success")
            }
        }
    }

    #[test]
    fn parse_down_json_removed() {
        let json = r#"{"outcome":"removed","containerId":"c1"}"#;
        let result = parse_down_json_output(json);
        match result {
            cella_protocol::DownOperationResult::Success { outcome, .. } => {
                assert!(matches!(
                    outcome,
                    cella_protocol::DownOutcome::Removed
                ));
            }
            cella_protocol::DownOperationResult::Error { .. } => {
                panic!("Expected Success")
            }
        }
    }

    #[test]
    fn parse_down_json_no_outcome_defaults_stopped() {
        let json = r#"{"containerId":"c1"}"#;
        let result = parse_down_json_output(json);
        match result {
            cella_protocol::DownOperationResult::Success { outcome, .. } => {
                assert!(matches!(
                    outcome,
                    cella_protocol::DownOutcome::Stopped
                ));
            }
            cella_protocol::DownOperationResult::Error { .. } => {
                panic!("Expected Success")
            }
        }
    }

    #[test]
    fn parse_down_json_invalid() {
        let result = parse_down_json_output("invalid json");
        assert!(matches!(
            result,
            cella_protocol::DownOperationResult::Error { .. }
        ));
    }

    #[test]
    fn parse_down_json_empty() {
        let result = parse_down_json_output("");
        assert!(matches!(
            result,
            cella_protocol::DownOperationResult::Error { .. }
        ));
    }

    #[test]
    fn parse_down_json_no_container_id() {
        let json = r#"{"outcome":"stopped"}"#;
        let result = parse_down_json_output(json);
        match result {
            cella_protocol::DownOperationResult::Success { container_name, .. } => {
                assert_eq!(container_name, "");
            }
            cella_protocol::DownOperationResult::Error { .. } => {
                panic!("Expected Success")
            }
        }
    }

    // ---------------------------------------------------------------
    // parse_up_json_output
    // ---------------------------------------------------------------

    #[test]
    fn parse_up_json_success() {
        let json = r#"{"containerId":"c1","workspaceFolder":"/workspace"}"#;
        let result = parse_up_json_output(json);
        match result {
            cella_protocol::WorktreeOperationResult::Success {
                container_name,
                worktree_path,
            } => {
                assert_eq!(container_name, "c1");
                assert_eq!(worktree_path, "/workspace");
            }
            cella_protocol::WorktreeOperationResult::Error { .. } => {
                panic!("Expected Success")
            }
        }
    }

    #[test]
    fn parse_up_json_missing_fields() {
        let json = r"{}";
        let result = parse_up_json_output(json);
        match result {
            cella_protocol::WorktreeOperationResult::Success {
                container_name,
                worktree_path,
            } => {
                assert_eq!(container_name, "");
                assert_eq!(worktree_path, "");
            }
            cella_protocol::WorktreeOperationResult::Error { .. } => {
                panic!("Expected Success with empty fields")
            }
        }
    }

    #[test]
    fn parse_up_json_invalid() {
        let result = parse_up_json_output("garbage");
        assert!(matches!(
            result,
            cella_protocol::WorktreeOperationResult::Error { .. }
        ));
    }

    #[test]
    fn parse_up_json_empty() {
        let result = parse_up_json_output("");
        assert!(matches!(
            result,
            cella_protocol::WorktreeOperationResult::Error { .. }
        ));
    }

    // ---------------------------------------------------------------
    // send_message
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn send_message_writes_json_newline() {
        let msg = DaemonMessage::PortMapping {
            container_port: 3000,
            host_port: 3000,
        };
        let mut output = Vec::<u8>::new();
        send_message(&mut output, &msg).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains("port_mapping"));
    }

    #[tokio::test]
    async fn send_message_credential_response() {
        let msg = DaemonMessage::CredentialResponse {
            id: "req-1".to_string(),
            fields: HashMap::new(),
        };
        let mut output = Vec::<u8>::new();
        send_message(&mut output, &msg).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("credential_response"));
        assert!(text.contains("req-1"));
    }

    // ---------------------------------------------------------------
    // send_reject
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn send_reject_writes_daemon_hello_with_error() {
        let mut output = Vec::<u8>::new();
        send_reject(&mut output, "test error".to_string()).await;
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("test error"));
        assert!(text.contains("protocol_version"));
        assert!(text.ends_with('\n'));
    }

    #[tokio::test]
    async fn send_reject_empty_error() {
        let mut output = Vec::<u8>::new();
        send_reject(&mut output, String::new()).await;
        let text = String::from_utf8(output).unwrap();
        // Should contain an error field even if empty
        let hello: DaemonHello = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(hello.error, Some(String::new()));
    }

    // ---------------------------------------------------------------
    // handle_agent_message — Health variant
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn handle_health_message_returns_none() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        let browser = Arc::new(BrowserHandler::new());
        let state = Arc::new(AgentConnectionState::new());
        let ctx = AgentHandlerContext {
            port_manager: &pm,
            browser_handler: &browser,
            container_id: Some("c1"),
            proxy_cmd_tx: None,
            container_ip: None,
        };

        let msg = AgentMessage::Health {
            uptime_secs: 100,
            ports_detected: 5,
        };
        let result = handle_agent_message(msg, &ctx, &state).await;
        assert!(result.is_none());
        // Should update state
        assert!(state.connected.load(Ordering::Relaxed));
        assert!(state.last_seen_secs.load(Ordering::Relaxed) > 0);
    }

    // ---------------------------------------------------------------
    // handle_agent_message — PortClosed variant
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn handle_port_closed_returns_none() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "test", Some("172.20.0.5".to_string()), vec![], None);
        }
        let browser = Arc::new(BrowserHandler::new());
        let state = Arc::new(AgentConnectionState::new());
        let ctx = AgentHandlerContext {
            port_manager: &pm,
            browser_handler: &browser,
            container_id: Some("c1"),
            proxy_cmd_tx: None,
            container_ip: None,
        };

        let msg = AgentMessage::PortClosed {
            port: 3000,
            protocol: PortProtocol::Tcp,
        };
        let result = handle_agent_message(msg, &ctx, &state).await;
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    // handle_agent_message — CredentialRequest
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn handle_credential_request_returns_response() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        let browser = Arc::new(BrowserHandler::new());
        let state = Arc::new(AgentConnectionState::new());
        let ctx = AgentHandlerContext {
            port_manager: &pm,
            browser_handler: &browser,
            container_id: Some("c1"),
            proxy_cmd_tx: None,
            container_ip: None,
        };

        // Use an unknown operation so it fails fast without needing real git
        let msg = AgentMessage::CredentialRequest {
            id: "req-42".to_string(),
            operation: "badop".to_string(),
            fields: HashMap::new(),
        };
        let result = handle_agent_message(msg, &ctx, &state).await;
        assert!(result.is_some());
        match result.unwrap() {
            DaemonMessage::CredentialResponse { id, fields } => {
                assert_eq!(id, "req-42");
                // Failed credential call returns empty fields
                assert!(fields.is_empty());
            }
            other => panic!("Expected CredentialResponse, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // handle_agent_message updates agent_state
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn agent_state_updated_on_message() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        let browser = Arc::new(BrowserHandler::new());
        let state = Arc::new(AgentConnectionState::new());
        assert!(!state.connected.load(Ordering::Relaxed));
        assert_eq!(state.last_seen_secs.load(Ordering::Relaxed), 0);

        let ctx = AgentHandlerContext {
            port_manager: &pm,
            browser_handler: &browser,
            container_id: Some("c1"),
            proxy_cmd_tx: None,
            container_ip: None,
        };

        let msg = AgentMessage::Health {
            uptime_secs: 0,
            ports_detected: 0,
        };
        let _ = handle_agent_message(msg, &ctx, &state).await;

        assert!(state.connected.load(Ordering::Relaxed));
        let ts = state.last_seen_secs.load(Ordering::Relaxed);
        assert!(ts > 0, "last_seen_secs should be non-zero after message");
    }

    // ---------------------------------------------------------------
    // rewrite_browser_url — additional edge cases
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn rewrite_url_no_scheme_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("localhost:3000/path", &pm, "c1").await;
        assert_eq!(result, "localhost:3000/path");
    }

    #[tokio::test]
    async fn rewrite_url_ipv6_localhost() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "a", Some("172.20.0.5".to_string()), vec![], None);
            guard.register_container("c2", "b", Some("172.20.0.6".to_string()), vec![], None);
            guard.handle_port_open("c1", 4000, PortProtocol::Tcp, None);
            guard.handle_port_open("c2", 4000, PortProtocol::Tcp, None);
        }
        let result = rewrite_browser_url("http://[::1]:4000/path", &pm, "c2").await;
        assert_eq!(result, "http://localhost:4001/path");
    }

    #[tokio::test]
    async fn rewrite_url_empty_string() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("", &pm, "c1").await;
        assert_eq!(result, "");
    }

    // ---------------------------------------------------------------
    // handle_port_open — without proxy
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn handle_port_open_returns_mapping() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "test", Some("172.20.0.5".to_string()), vec![], None);
        }
        let browser = Arc::new(BrowserHandler::new());
        let state = Arc::new(AgentConnectionState::new());
        let ctx = AgentHandlerContext {
            port_manager: &pm,
            browser_handler: &browser,
            container_id: Some("c1"),
            proxy_cmd_tx: None,
            container_ip: None,
        };

        let msg = AgentMessage::PortOpen {
            port: 3000,
            protocol: PortProtocol::Tcp,
            process: None,
            bind: cella_protocol::BindAddress::All,
            proxy_port: None,
        };
        let result = handle_agent_message(msg, &ctx, &state).await;
        // Without proxy_cmd_tx, should return PortMapping
        match result {
            Some(DaemonMessage::PortMapping {
                container_port,
                host_port,
            }) => {
                assert_eq!(container_port, 3000);
                assert_eq!(host_port, 3000);
            }
            other => panic!("Expected PortMapping, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // snapshot_binary
    // ---------------------------------------------------------------

    #[test]
    fn snapshot_binary_nonexistent_source() {
        let result = snapshot_binary(std::path::Path::new("/nonexistent/binary/path"));
        assert!(result.is_none());
    }
}
