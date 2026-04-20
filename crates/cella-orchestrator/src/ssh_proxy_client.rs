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
