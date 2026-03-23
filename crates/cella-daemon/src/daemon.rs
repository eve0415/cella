//! Daemon lifecycle: PID file, daemonization, liveness check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::CellaDaemonError;
use crate::browser::BrowserHandler;
use crate::control_server::current_time_secs;
use crate::health::run_health_monitor;
use crate::management::{ManagementContext, run_management_server};
use crate::orbstack;
use crate::port_manager::PortManager;
use crate::proxy::run_proxy_coordinator;

/// Write the PID file and ensure the parent directory exists.
fn write_pid_and_ensure_dir(socket_path: &Path, pid_path: &Path) -> Result<u32, CellaDaemonError> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CellaDaemonError::PidFile {
            message: format!("failed to create directory {}: {e}", parent.display()),
        })?;
    }

    let pid = std::process::id();
    std::fs::write(pid_path, pid.to_string()).map_err(|e| CellaDaemonError::PidFile {
        message: format!("failed to write PID file: {e}"),
    })?;
    info!("Cella daemon started (PID {pid})");
    Ok(pid)
}

/// Write the daemon.control file with port and auth token.
fn write_control_file(
    control_socket_path: &Path,
    control_port: u16,
    auth_token: &str,
) -> Result<PathBuf, CellaDaemonError> {
    let control_file_path = control_socket_path.with_file_name("daemon.control");
    std::fs::write(&control_file_path, format!("{control_port}\n{auth_token}")).map_err(|e| {
        CellaDaemonError::PidFile {
            message: format!("failed to write daemon.control: {e}"),
        }
    })?;
    info!("Control TCP server on 127.0.0.1:{control_port}");
    Ok(control_file_path)
}

/// Run the unified cella daemon.
///
/// Starts the control server, legacy credential servers, and health monitor.
///
/// # Errors
///
/// Returns error if socket binding or PID file creation fails.
pub async fn run_daemon(
    socket_path: &Path,
    pid_path: &Path,
    port_path: &Path,
    control_socket_path: &Path,
) -> Result<(), CellaDaemonError> {
    write_pid_and_ensure_dir(socket_path, pid_path)?;

    // Generate auth token for agent connections
    let auth_token = generate_auth_token();

    // Bind TCP listener for agent control connections
    let control_listener = bind_control_tcp(control_socket_path).await?;
    let control_port = control_listener
        .local_addr()
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("failed to get control TCP local addr: {e}"),
        })?
        .port();

    // Persist port+token to daemon.control for reclaiming on restart
    let control_file_path = write_control_file(control_socket_path, control_port, &auth_token)?;

    let last_activity = Arc::new(AtomicU64::new(current_time_secs()));
    let is_orbstack = orbstack::is_orbstack();
    let port_manager = Arc::new(tokio::sync::Mutex::new(
        PortManager::new(is_orbstack).with_port_checker(crate::port_manager::is_host_port_free),
    ));
    let browser_handler = Arc::new(BrowserHandler::new());

    // Spawn health monitor
    let health_activity = last_activity.clone();
    let health_pid = pid_path.to_path_buf();
    let health_socket = socket_path.to_path_buf();
    tokio::spawn(async move {
        run_health_monitor(health_activity, &health_pid, &health_socket).await;
    });

    // Spawn legacy credential proxy servers (TCP + Unix socket)
    let legacy_activity = last_activity.clone();
    let legacy_socket = socket_path.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = run_legacy_credential_server(&legacy_socket, legacy_activity).await {
            warn!("Legacy credential server error: {e}");
        }
    });

    let tcp_activity = last_activity.clone();
    let port_path_owned = port_path.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = run_legacy_tcp_server(&port_path_owned, tcp_activity).await {
            warn!("Legacy TCP server error: {e}");
        }
    });

    // Spawn proxy coordinator
    let (proxy_cmd_tx, proxy_cmd_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        run_proxy_coordinator(proxy_cmd_rx).await;
    });

    let start_time = std::time::Instant::now();
    let daemon_started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let ctx = ManagementContext {
        last_activity,
        port_manager,
        browser_handler,
        proxy_cmd_tx,
        start_time,
        is_orbstack,
        daemon_started_at,
        shutdown_tx,
        auth_token,
        control_port,
    };

    // Run the management server (CLI protocol) — blocks until shutdown
    let result =
        run_management_server(control_socket_path, ctx, shutdown_rx, control_listener).await;

    // Clean up on exit with a 5s timeout
    let cleanup_fut = tokio::task::spawn_blocking({
        let pid = pid_path.to_path_buf();
        let sock = socket_path.to_path_buf();
        let port = port_path.to_path_buf();
        let ctrl = control_socket_path.to_path_buf();
        let ctrl_file = control_file_path;
        move || {
            cleanup(&pid, &sock, &port, &ctrl);
            let _ = std::fs::remove_file(&ctrl_file);
        }
    });
    if tokio::time::timeout(Duration::from_secs(5), cleanup_fut)
        .await
        .is_err()
    {
        info!("Cleanup timed out after 5s, exiting");
        std::process::exit(0);
    }

    result
}

/// Generate a hex-encoded random auth token.
fn generate_auth_token() -> String {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in &buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Bind a TCP listener for agent control connections.
///
/// Attempts to reclaim the port from a previous daemon run (persisted in `daemon.control`).
async fn bind_control_tcp(
    control_socket_path: &Path,
) -> Result<tokio::net::TcpListener, CellaDaemonError> {
    use std::net::SocketAddr;

    let control_file = control_socket_path.with_file_name("daemon.control");
    let preferred_port = std::fs::read_to_string(&control_file)
        .ok()
        .and_then(|s| s.lines().next().and_then(|l| l.trim().parse::<u16>().ok()))
        .unwrap_or(0);

    if preferred_port != 0 {
        let addr: SocketAddr = ([127, 0, 0, 1], preferred_port).into();
        if let Ok(l) = tokio::net::TcpListener::bind(addr).await {
            debug!("Reclaimed control TCP port {preferred_port}");
            return Ok(l);
        }
        warn!("Cannot reclaim control TCP port {preferred_port}, binding new port");
    }

    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("failed to bind control TCP: {e}"),
        })
}

/// Legacy credential proxy Unix socket server (backward compatibility).
async fn run_legacy_credential_server(
    socket_path: &Path,
    last_activity: Arc<AtomicU64>,
) -> Result<(), CellaDaemonError> {
    use std::sync::atomic::Ordering;
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).map_err(|e| CellaDaemonError::Socket {
        message: format!("failed to bind {}: {e}", socket_path.display()),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(socket_path, perms);
    }

    info!(
        "Legacy credential server listening on {}",
        socket_path.display()
    );

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                tokio::spawn(async move {
                    if let Err(e) = handle_legacy_stream(stream).await {
                        warn!("Legacy connection error: {e}");
                    }
                });
            }
            Err(e) => warn!("Legacy accept error: {e}"),
        }
    }
}

/// Handle a legacy credential proxy connection.
async fn handle_legacy_stream(
    mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
) -> Result<(), CellaDaemonError> {
    use crate::credential::{
        CredentialResponse, format_response, invoke_git_credential, parse_request,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 8192];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("read error: {e}"),
        })?;

    if n == 0 {
        return Ok(());
    }

    let data = String::from_utf8_lossy(&buf[..n]);
    let request = parse_request(&data)?;

    if request.operation == "ping" {
        stream
            .write_all(b"pong\n")
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("write error: {e}"),
            })?;
        return Ok(());
    }

    let operation = request.operation.clone();
    let fields = request.fields.clone();

    let result = tokio::task::spawn_blocking(move || invoke_git_credential(&operation, &fields))
        .await
        .map_err(|e| CellaDaemonError::GitCredential {
            message: format!("task join error: {e}"),
        })?;

    match result {
        Ok(response_fields) => {
            let response = CredentialResponse {
                fields: response_fields,
            };
            let output = format_response(&response);
            stream
                .write_all(output.as_bytes())
                .await
                .map_err(|e| CellaDaemonError::Socket {
                    message: format!("write error: {e}"),
                })?;
        }
        Err(e) => {
            warn!("git credential error: {e}");
            stream
                .write_all(b"\n")
                .await
                .map_err(|e| CellaDaemonError::Socket {
                    message: format!("write error: {e}"),
                })?;
        }
    }

    Ok(())
}

/// Bind a legacy TCP listener, attempting to reclaim a previously used port.
async fn bind_legacy_tcp(port_path: &Path) -> Result<tokio::net::TcpListener, CellaDaemonError> {
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    let preferred_port = std::fs::read_to_string(port_path)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(0);

    if preferred_port != 0 {
        let addr: SocketAddr = ([127, 0, 0, 1], preferred_port).into();
        if let Ok(l) = TcpListener::bind(addr).await {
            debug!("Reusing previous TCP port {preferred_port}");
            return Ok(l);
        }
        warn!("Cannot reclaim TCP port {preferred_port}, binding new port");
    }

    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    TcpListener::bind(addr)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("failed to bind TCP: {e}"),
        })
}

/// Write the port number to a file so clients can discover it.
fn write_legacy_port_file(port_path: &Path, port: u16) -> Result<(), CellaDaemonError> {
    if let Some(parent) = port_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(port_path, port.to_string()).map_err(|e| CellaDaemonError::PidFile {
        message: format!("failed to write port file: {e}"),
    })
}

/// Legacy TCP credential server (for VM-based runtimes).
async fn run_legacy_tcp_server(
    port_path: &Path,
    last_activity: Arc<AtomicU64>,
) -> Result<(), CellaDaemonError> {
    use std::sync::atomic::Ordering;

    let listener = bind_legacy_tcp(port_path).await?;

    let port = listener
        .local_addr()
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("failed to get local addr: {e}"),
        })?
        .port();

    write_legacy_port_file(port_path, port)?;

    info!("Legacy TCP credential server on 127.0.0.1:{port}");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                debug!("TCP connection from {peer}");
                tokio::spawn(async move {
                    if let Err(e) = handle_legacy_stream(stream).await {
                        warn!("TCP handler error: {e}");
                    }
                });
            }
            Err(e) => warn!("TCP accept error: {e}"),
        }
    }
}

/// Check if the daemon is already running.
pub fn is_daemon_running(pid_path: &Path, socket_path: &Path) -> bool {
    let Some(pid) = read_pid_file(pid_path) else {
        return false;
    };

    let alive = is_process_alive(pid);
    if !alive {
        debug!("Stale PID file found (PID {pid}), cleaning up");
        let _ = std::fs::remove_file(pid_path);
        let _ = std::fs::remove_file(socket_path);
        return false;
    }

    socket_path.exists()
}

/// Start the daemon as a detached background process.
///
/// # Errors
///
/// Returns error if the daemon process cannot be spawned.
pub fn start_daemon_background(
    socket_path: &Path,
    pid_path: &Path,
    port_path: &Path,
    control_socket_path: &Path,
) -> Result<(), CellaDaemonError> {
    let exe = std::env::current_exe().map_err(|e| CellaDaemonError::PidFile {
        message: format!("failed to get current exe: {e}"),
    })?;

    std::process::Command::new(exe)
        .args([
            "daemon",
            "start",
            "--socket",
            &socket_path.to_string_lossy(),
            "--pid-file",
            &pid_path.to_string_lossy(),
            "--port-file",
            &port_path.to_string_lossy(),
            "--control-socket",
            &control_socket_path.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| CellaDaemonError::PidFile {
            message: format!("failed to spawn daemon: {e}"),
        })?;

    info!("Cella daemon started in background");
    Ok(())
}

/// Ensure the daemon is running. Start it if not.
///
/// # Errors
///
/// Returns error if the daemon cannot be started.
pub fn ensure_daemon_running(
    socket_path: &Path,
    pid_path: &Path,
    port_path: &Path,
    control_socket_path: &Path,
) -> Result<PathBuf, CellaDaemonError> {
    if is_daemon_running(pid_path, socket_path) {
        debug!("Cella daemon already running");
        return Ok(socket_path.to_path_buf());
    }

    start_daemon_background(socket_path, pid_path, port_path, control_socket_path)?;

    // Brief wait for the daemon to create its socket
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return Ok(socket_path.to_path_buf());
        }
    }

    warn!("Cella daemon started but socket not yet available");
    Ok(socket_path.to_path_buf())
}

/// Stop the running daemon.
///
/// # Errors
///
/// Returns `CellaDaemonError::NotRunning` if no daemon is running.
pub fn stop_daemon(
    pid_path: &Path,
    socket_path: &Path,
    port_path: &Path,
    control_socket_path: &Path,
) -> Result<(), CellaDaemonError> {
    let pid = read_pid_file(pid_path).ok_or(CellaDaemonError::NotRunning)?;

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }

    cleanup(pid_path, socket_path, port_path, control_socket_path);
    info!("Cella daemon stopped");
    Ok(())
}

fn read_pid_file(pid_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse().ok()
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        // Signal 0 checks process existence without sending a signal.
        // EPERM means the process exists but we lack permission — still alive.
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        kill(Pid::from_raw(pid), None).is_ok()
            || matches!(nix::errno::Errno::last(), nix::errno::Errno::EPERM)
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn cleanup(pid_path: &Path, socket_path: &Path, port_path: &Path, control_socket_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(port_path);
    let _ = std::fs::remove_file(control_socket_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        std::fs::write(&pid_path, "12345").unwrap();
        assert_eq!(read_pid_file(&pid_path), Some(12345));
    }

    #[test]
    fn read_pid_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pid_file(&dir.path().join("nope.pid")), None);
    }

    #[test]
    fn daemon_not_running_without_pid() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_daemon_running(
            &dir.path().join("test.pid"),
            &dir.path().join("test.sock"),
        ));
    }
}
