//! Daemon RPC for SSH-agent TCP-bridge registration.
//!
//! Bridges `cella_env::SshAgentProxyRequest` (a Tier-3 description of a
//! pending colima proxy) into a daemon-managed TCP listener that the
//! in-container `cella-agent` connects to. Cella-env (Tier 3) cannot
//! depend on the daemon RPC client; the orchestrator (Tier 2) makes the
//! call here and converts the response into the env vars the rest of
//! the up pipeline needs (`SSH_AUTH_SOCK` for the container's view,
//! `CELLA_SSH_AGENT_BRIDGE` so the in-container agent knows where to
//! connect).
//!
//! The earlier design (host-side Unix socket bind-mounted into the
//! container) failed empirically on colima because virtiofs rejects
//! `mkdir` on any host-side socket path. TCP sidesteps that entire
//! mount layer.

use std::path::Path;

use cella_daemon_client::DaemonClient;
use cella_env::{ForwardEnv, SshAgentProxyRequest};
use tracing::warn;

/// Outcome of a successful bridge registration.
pub struct ResolvedSshProxy {
    /// Env vars to inject into the container so consumers see a normal
    /// `SSH_AUTH_SOCK` and the in-container agent knows which TCP port
    /// to bridge through.
    pub env: Vec<ForwardEnv>,
    /// Refcount returned by the daemon; `1` means a fresh bridge was
    /// created, `>1` means an existing one was reused.
    pub refcount: usize,
    /// Localhost TCP port the daemon bound the bridge on. Used for
    /// the CLI status print (`ssh-agent proxy: bridged via host:<port>`).
    pub bridge_port: u16,
}

/// Resolve a colima proxy request via the daemon. Returns `None` when:
/// - The daemon socket isn't reachable
/// - The daemon's RPC handler returns an error or unexpected variant
///
/// Per the design, a `None` here causes the orchestrator to skip SSH
/// forwarding entirely rather than ship the container half a setup. The
/// caller should log the underlying reason via the warnings emitted here.
///
/// `host_gateway` is the hostname the container uses to reach the daemon
/// — typically `host.docker.internal` (Docker Desktop, `OrbStack`, recent
/// colima) or `host.local` (Apple Container).
pub async fn register_proxy(
    daemon_socket: &Path,
    workspace: &Path,
    host_gateway: &str,
    request: &SshAgentProxyRequest,
) -> Option<ResolvedSshProxy> {
    let client = DaemonClient::new(daemon_socket);
    match client
        .register_ssh_agent_proxy(
            workspace.to_string_lossy().into_owned(),
            request.upstream_socket.to_string_lossy().into_owned(),
        )
        .await
    {
        Ok(registration) => Some(ResolvedSshProxy {
            env: vec![
                ForwardEnv {
                    key: "SSH_AUTH_SOCK".to_string(),
                    value: request.env_value.clone(),
                },
                ForwardEnv {
                    key: "CELLA_SSH_AGENT_BRIDGE".to_string(),
                    value: format!("{host_gateway}:{}", registration.bridge_port),
                },
                ForwardEnv {
                    key: "CELLA_SSH_AGENT_TARGET".to_string(),
                    value: request.mount_target.clone(),
                },
            ],
            refcount: registration.refcount,
            bridge_port: registration.bridge_port,
        }),
        Err(e) => {
            warn!(
                "ssh-agent bridge: daemon RegisterSshAgentProxy failed for {}: {e}",
                workspace.display()
            );
            None
        }
    }
}

/// Decrement the refcount for `workspace`. Best-effort: never fails the
/// caller's down flow even if the daemon is unreachable.
pub async fn release_proxy(daemon_socket: &Path, workspace: &Path) {
    let client = DaemonClient::new(daemon_socket);
    if let Err(e) = client
        .release_ssh_agent_proxy(workspace.to_string_lossy().into_owned())
        .await
    {
        warn!(
            "ssh-agent bridge: daemon ReleaseSshAgentProxy failed for {}: {e}",
            workspace.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use cella_protocol::ManagementResponse;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    /// Stand up a mock daemon on a Unix socket that responds to one
    /// `ManagementRequest` with the canned `response`. Records the
    /// received request bytes for assertion. Returns a guard holding
    /// the recorded request and a `JoinHandle` to abort.
    fn spawn_mock_daemon(
        socket_path: &Path,
        response: ManagementResponse,
    ) -> (Arc<Mutex<Option<String>>>, tokio::task::JoinHandle<()>) {
        let listener = UnixListener::bind(socket_path).expect("bind mock daemon socket");
        let received = Arc::new(Mutex::new(None));
        let received_for_task = received.clone();
        let task = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = tokio::io::split(stream);
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_ok() {
                *received_for_task.lock().await = Some(line.trim().to_string());
            }
            let mut json = serde_json::to_string(&response).unwrap();
            json.push('\n');
            let _ = writer.write_all(json.as_bytes()).await;
            let _ = writer.flush().await;
        });
        (received, task)
    }

    fn sample_request() -> SshAgentProxyRequest {
        SshAgentProxyRequest {
            upstream_socket: PathBuf::from("/Users/me/.1password/agent.sock"),
            mount_target: "/run/host-services/ssh-auth.sock".to_string(),
            env_value: "/run/host-services/ssh-auth.sock".to_string(),
        }
    }

    #[tokio::test]
    async fn register_proxy_translates_response_into_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("daemon.sock");
        let (received, task) = spawn_mock_daemon(
            &daemon_sock,
            ManagementResponse::SshAgentProxyRegistered {
                bridge_port: 54321,
                refcount: 1,
            },
        );

        let workspace = PathBuf::from("/Users/me/proj");
        let req = sample_request();
        let resolved = register_proxy(&daemon_sock, &workspace, "host.docker.internal", &req)
            .await
            .expect("happy-path register must return Some");

        assert_eq!(resolved.bridge_port, 54321);
        assert_eq!(resolved.refcount, 1);

        // The three injected env vars: SSH_AUTH_SOCK (consumer-facing),
        // CELLA_SSH_AGENT_BRIDGE (where to connect), CELLA_SSH_AGENT_TARGET
        // (where the in-container agent should bind the unix socket).
        let by_key: std::collections::HashMap<_, _> = resolved
            .env
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();
        assert_eq!(
            by_key.get("SSH_AUTH_SOCK").map(String::as_str),
            Some("/run/host-services/ssh-auth.sock")
        );
        assert_eq!(
            by_key.get("CELLA_SSH_AGENT_BRIDGE").map(String::as_str),
            Some("host.docker.internal:54321")
        );
        assert_eq!(
            by_key.get("CELLA_SSH_AGENT_TARGET").map(String::as_str),
            Some("/run/host-services/ssh-auth.sock")
        );

        // Outgoing JSON shape matches the protocol contract — guards
        // against silent serde renames in cella-protocol.
        let raw = received
            .lock()
            .await
            .clone()
            .expect("daemon must have received a request");
        assert!(raw.contains("\"type\":\"register_ssh_agent_proxy\""));
        assert!(raw.contains("\"workspace\":\"/Users/me/proj\""));
        assert!(raw.contains("\"upstream_socket\":\"/Users/me/.1password/agent.sock\""));

        task.abort();
    }

    #[tokio::test]
    async fn register_proxy_returns_none_when_daemon_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("daemon.sock");
        let (_received, task) = spawn_mock_daemon(
            &daemon_sock,
            ManagementResponse::Error {
                message: "bind failed: EADDRINUSE".to_string(),
            },
        );

        let resolved = register_proxy(
            &daemon_sock,
            &PathBuf::from("/x"),
            "host.docker.internal",
            &sample_request(),
        )
        .await;
        assert!(
            resolved.is_none(),
            "Error response must surface as None so orchestrator skips forwarding"
        );

        task.abort();
    }

    #[tokio::test]
    async fn register_proxy_returns_none_when_daemon_returns_unexpected_variant() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("daemon.sock");
        let (_received, task) = spawn_mock_daemon(&daemon_sock, ManagementResponse::Pong);

        let resolved = register_proxy(
            &daemon_sock,
            &PathBuf::from("/x"),
            "host.docker.internal",
            &sample_request(),
        )
        .await;
        assert!(
            resolved.is_none(),
            "wrong response variant must surface as None"
        );

        task.abort();
    }

    #[tokio::test]
    async fn register_proxy_returns_none_when_daemon_socket_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("nonexistent.sock");
        let resolved = register_proxy(
            &daemon_sock,
            &PathBuf::from("/x"),
            "host.docker.internal",
            &sample_request(),
        )
        .await;
        assert!(
            resolved.is_none(),
            "daemon-not-running must surface as None"
        );
    }

    #[tokio::test]
    async fn release_proxy_sends_correct_request_shape() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("daemon.sock");
        let (received, task) = spawn_mock_daemon(
            &daemon_sock,
            ManagementResponse::SshAgentProxyReleased { torn_down: true },
        );

        release_proxy(&daemon_sock, &PathBuf::from("/Users/me/proj")).await;

        let raw = received
            .lock()
            .await
            .clone()
            .expect("daemon must have received a request");
        assert!(raw.contains("\"type\":\"release_ssh_agent_proxy\""));
        assert!(raw.contains("\"workspace\":\"/Users/me/proj\""));

        task.abort();
    }

    #[tokio::test]
    async fn release_proxy_does_not_panic_when_daemon_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("nonexistent.sock");
        release_proxy(&daemon_sock, &PathBuf::from("/x")).await;
    }
}
