//! Per-exec TCP stream bridge for TTY forwarding.
//!
//! Opens a short-lived TCP listener on a random port, accepts one connection,
//! and bidirectionally forwards bytes between the accepted TCP stream and a
//! PTY master file descriptor. Used by `cella switch` to provide interactive
//! shell sessions through the daemon.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tracing::{debug, error, info};

/// Result of a completed stream bridge session.
pub struct StreamSession {
    /// The TCP port the listener was bound to.
    pub port: u16,
    /// Handle to await session completion and retrieve exit code.
    pub handle: tokio::task::JoinHandle<i32>,
}

/// Start a PTY-backed stream bridge for an interactive docker exec session.
///
/// 1. Allocates a PTY via `portable-pty`
/// 2. Spawns `docker exec -it <container> <shell>` in the PTY slave
/// 3. Opens a TCP listener on `bind_addr:0`
/// 4. Returns the port and a handle; the handle resolves when the session ends
///
/// The caller sends the port to the agent via `StreamReady`, and the agent
/// connects to forward its stdin/stdout.
///
/// # Errors
///
/// Returns an error if PTY allocation, command spawn, or TCP bind fails.
pub fn start_stream_bridge(
    container_name: &str,
    bind_addr: &str,
) -> Result<StreamSession, Box<dyn std::error::Error + Send + Sync>> {
    let pty_system = NativePtySystem::default();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("docker");
    cmd.args([
        "exec",
        "-it",
        container_name,
        "sh",
        "-c",
        "exec $SHELL -l 2>/dev/null || exec sh",
    ]);

    let _child = pair.slave.spawn_command(cmd)?;
    // Drop slave so the master gets EOF when the child exits.
    drop(pair.slave);

    let listener = TcpListener::bind(format!("{bind_addr}:0"))?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(false)?;

    info!(port, container = container_name, "stream bridge listening");

    let master = pair.master;
    let handle = tokio::task::spawn_blocking(move || run_bridge(&listener, &*master));

    Ok(StreamSession { port, handle })
}

/// Accept one connection and forward bytes between it and the PTY master.
///
/// Returns the exit code (0 on clean disconnect, 1 on error).
fn run_bridge(listener: &TcpListener, master: &(dyn MasterPty + Send)) -> i32 {
    // Wait for agent to connect (with timeout).
    let Some(stream) = accept_with_timeout(listener, Duration::from_mins(1)) else {
        error!("stream bridge: no connection within 60s");
        return 1;
    };

    debug!("stream bridge: agent connected");

    let mut reader = master
        .try_clone_reader()
        .expect("failed to clone PTY reader");
    let mut writer = master.take_writer().expect("failed to take PTY writer");

    // TCP -> PTY (agent stdin -> docker exec stdin)
    let mut stream_read = stream.try_clone().expect("failed to clone TCP stream");
    let tcp_to_pty = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stream_read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // PTY -> TCP (docker exec stdout -> agent stdout)
    let mut stream_write = stream;
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream_write.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }

    let _ = tcp_to_pty.join();
    debug!("stream bridge: session ended");
    0
}

fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> Option<std::net::TcpStream> {
    listener.set_nonblocking(true).ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // accept_with_timeout
    // ---------------------------------------------------------------

    #[test]
    fn accept_with_timeout_returns_none_on_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Very short timeout — no one will connect.
        let result = accept_with_timeout(&listener, Duration::from_millis(150));
        assert!(result.is_none());
    }

    #[test]
    fn accept_with_timeout_returns_stream_on_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect from another thread.
        let handle = std::thread::spawn(move || std::net::TcpStream::connect(addr).unwrap());

        let result = accept_with_timeout(&listener, Duration::from_secs(5));
        assert!(result.is_some());

        let _ = handle.join();
    }

    #[test]
    fn accept_with_timeout_receives_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream.write_all(b"hello").unwrap();
        });

        let result = accept_with_timeout(&listener, Duration::from_secs(5));
        assert!(result.is_some());
        let mut stream = result.unwrap();
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        let _ = handle.join();
    }

    // ---------------------------------------------------------------
    // StreamSession struct
    // ---------------------------------------------------------------

    #[test]
    fn stream_session_port_field() {
        // StreamSession is just a data holder — test that it can be constructed.
        let handle = tokio::runtime::Runtime::new().unwrap().spawn(async { 42 });
        let session = StreamSession { port: 8080, handle };
        assert_eq!(session.port, 8080);
    }

    // ---------------------------------------------------------------
    // start_stream_bridge error cases
    // ---------------------------------------------------------------

    // We can test that binding to an invalid address fails.
    // This doesn't require Docker — it will fail at TCP bind or PTY allocation
    // depending on the platform.
    #[tokio::test]
    async fn start_stream_bridge_nonexistent_container() {
        // The function uses tokio::task::spawn_blocking internally,
        // so it must be called from within a Tokio runtime.
        let result = start_stream_bridge("nonexistent-container-12345", "127.0.0.1");
        // On CI with Docker: PTY allocates, docker exec spawns (fails later), TCP binds -> Ok
        // On CI without Docker: spawn_command fails -> Err
        // Either is valid — just don't panic.
        if let Ok(session) = result {
            session.handle.abort();
        }
    }
}
