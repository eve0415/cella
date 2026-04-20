//! Daemon RPC for SSH-agent proxy registration.
//!
//! Bridges `cella_env::SshAgentProxyRequest` (a Tier-3 description of a
//! pending colima proxy) into a daemon-managed proxy socket the
//! orchestrator can bind-mount into the container. Cella-env (Tier 3)
//! cannot depend on the daemon RPC client; the orchestrator (Tier 2)
//! makes the call here and converts the response back into the standard
//! `ForwardMount` / `ForwardEnv` entries that the rest of the up pipeline
//! consumes.

use std::path::Path;

use cella_env::{ForwardEnv, ForwardMount, SshAgentProxyRequest};
use cella_protocol::{ManagementRequest, ManagementResponse};
use tracing::warn;

/// Outcome of a successful proxy registration.
pub struct ResolvedSshProxy {
    /// Mount entry to append to `EnvForwarding::mounts` — bind-mounts the
    /// daemon-supplied host socket into the container.
    pub mount: ForwardMount,
    /// `SSH_AUTH_SOCK` env var to set in the container.
    pub env: ForwardEnv,
    /// Refcount returned by the daemon; `1` means a fresh proxy was
    /// created, `>1` means an existing one was reused.
    pub refcount: usize,
    /// Host-side proxy socket path the daemon bound — useful for the
    /// CLI status print (`ssh-agent proxy: bridged to <path>`).
    pub proxy_socket: String,
}

/// Resolve a colima proxy request via the daemon. Returns `None` when:
/// - The daemon socket isn't reachable
/// - The daemon's RPC handler returns an error or unexpected variant
///
/// Per the design, a `None` here causes the orchestrator to skip SSH
/// forwarding entirely rather than mount a dead socket. The caller
/// should log the underlying reason via the warnings emitted here.
pub async fn register_proxy(
    daemon_socket: &Path,
    workspace: &Path,
    request: &SshAgentProxyRequest,
) -> Option<ResolvedSshProxy> {
    let req = ManagementRequest::RegisterSshAgentProxy {
        workspace: workspace.to_string_lossy().into_owned(),
        upstream_socket: request.upstream_socket.to_string_lossy().into_owned(),
    };
    let response =
        match cella_daemon::management::send_management_request(daemon_socket, &req).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "ssh-agent proxy: daemon RegisterSshAgentProxy failed for {}: {e}",
                    workspace.display()
                );
                return None;
            }
        };
    match response {
        ManagementResponse::SshAgentProxyRegistered {
            proxy_socket,
            refcount,
        } => Some(ResolvedSshProxy {
            mount: ForwardMount {
                source: proxy_socket.clone(),
                target: request.mount_target.clone(),
            },
            env: ForwardEnv {
                key: "SSH_AUTH_SOCK".to_string(),
                value: request.env_value.clone(),
            },
            refcount,
            proxy_socket,
        }),
        ManagementResponse::Error { message } => {
            warn!("ssh-agent proxy: daemon refused register: {message}");
            None
        }
        other => {
            warn!("ssh-agent proxy: daemon returned unexpected response: {other:?}");
            None
        }
    }
}

/// Decrement the refcount for `workspace`. Best-effort: never fails the
/// caller's down flow even if the daemon is unreachable.
pub async fn release_proxy(daemon_socket: &Path, workspace: &Path) {
    let req = ManagementRequest::ReleaseSshAgentProxy {
        workspace: workspace.to_string_lossy().into_owned(),
    };
    if let Err(e) = cella_daemon::management::send_management_request(daemon_socket, &req).await {
        warn!(
            "ssh-agent proxy: daemon ReleaseSshAgentProxy failed for {}: {e}",
            workspace.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

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
    async fn register_proxy_translates_response_into_resolved_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_sock = dir.path().join("daemon.sock");
        let (received, task) = spawn_mock_daemon(
            &daemon_sock,
            ManagementResponse::SshAgentProxyRegistered {
                proxy_socket: "/Users/me/.cella/run/ssh-agent-deadbeefcafef00d.sock".to_string(),
                refcount: 1,
            },
        );

        let workspace = PathBuf::from("/Users/me/proj");
        let req = sample_request();
        let resolved = register_proxy(&daemon_sock, &workspace, &req)
            .await
            .expect("happy-path register must return Some");

        // Response shape preserved.
        assert_eq!(
            resolved.proxy_socket,
            "/Users/me/.cella/run/ssh-agent-deadbeefcafef00d.sock"
        );
        assert_eq!(resolved.refcount, 1);

        // Mount + env entries built from request + response.
        assert_eq!(
            resolved.mount.source,
            "/Users/me/.cella/run/ssh-agent-deadbeefcafef00d.sock"
        );
        assert_eq!(resolved.mount.target, "/run/host-services/ssh-auth.sock");
        assert_eq!(resolved.env.key, "SSH_AUTH_SOCK");
        assert_eq!(resolved.env.value, "/run/host-services/ssh-auth.sock");

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

        let resolved = register_proxy(&daemon_sock, &PathBuf::from("/x"), &sample_request()).await;
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

        let resolved = register_proxy(&daemon_sock, &PathBuf::from("/x"), &sample_request()).await;
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
        // No spawn_mock_daemon — socket does not exist.

        let resolved = register_proxy(&daemon_sock, &PathBuf::from("/x"), &sample_request()).await;
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
        // Must not panic, must not return an error to caller — best-effort.
        release_proxy(&daemon_sock, &PathBuf::from("/x")).await;
    }
}
