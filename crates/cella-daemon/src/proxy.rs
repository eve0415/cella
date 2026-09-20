//! TCP proxy implementation for forwarding host ports to container ports.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cella_protocol::{DaemonMessage, ForwardHealth};
use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::control_server::ContainerHandle;
use crate::tunnel::TunnelBroker;

/// Commands for the proxy coordinator.
pub enum ProxyCommand {
    /// Start a new TCP proxy.
    Start {
        host_port: u16,
        target: ProxyStartTarget,
        /// Receives `Ok(())` if proxy bound successfully, or the bind error.
        result_tx: Option<tokio::sync::oneshot::Sender<Result<(), io::Error>>>,
    },
    /// Stop a running TCP proxy.
    Stop { host_port: u16 },
}

/// Target specification for starting a proxy.
#[derive(Debug, Clone)]
pub enum ProxyStartTarget {
    /// Connect directly to an `IP:port` (`OrbStack`, Linux native).
    DirectIp { ip: String, port: u16 },
    /// Tunnel through the agent via a reverse TCP connection.
    ///
    /// `target_host` overrides the hostname the agent connects to inside the
    /// container. `None` means `localhost` (the existing numeric-port path).
    /// Set to a Compose service name (e.g. `"db"`) for cross-service forwarding.
    AgentTunnel {
        container_name: String,
        port: u16,
        target_host: Option<String>,
    },
}

/// Handle to a running TCP proxy task.
pub struct ProxyHandle {
    handle: tokio::task::JoinHandle<()>,
    dial_state: Arc<DialFailureState>,
}

impl ProxyHandle {
    /// Abort the proxy task.
    pub fn abort(self) {
        self.handle.abort();
    }
}

/// Shared state needed by the proxy coordinator for tunnel mode.
pub struct ProxyCoordinatorContext {
    pub tunnel_broker: Arc<TunnelBroker>,
    pub container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
}

/// How a forward reaches the container.
#[derive(Clone, Copy)]
enum DialPath {
    /// Straight to the container's own address.
    Direct,
    /// Over the reverse tunnel the agent holds open.
    Tunnel,
}

/// Last observed delivery state of every running forward, keyed by host port.
pub type ForwardHealthTable = Arc<Mutex<HashMap<u16, Arc<DialFailureState>>>>;

/// Create an empty forward-health table.
#[must_use]
pub fn new_health_table() -> ForwardHealthTable {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Per-proxy record of what connections through a forward actually did.
#[derive(Default)]
pub struct DialFailureState {
    /// A failure of any kind has been logged since the last success.
    reported: AtomicBool,
    /// A broken path has been explained since the last success.
    diagnosed: AtomicBool,
    /// At least one connection has been attempted through this forward.
    observed: AtomicBool,
}

/// What a failure is worth saying, given what has already been said.
enum DialReport {
    /// Explain a broken path, once per run of failures.
    Diagnose,
    /// Report an ordinary failure, once per run of failures.
    Failure,
    /// Say nothing new.
    Repeat,
}

impl DialFailureState {
    /// What connections through this forward last did.
    pub fn health(&self) -> ForwardHealth {
        if !self.observed.load(Ordering::Relaxed) {
            return ForwardHealth::Unknown;
        }
        if self.reported.load(Ordering::Relaxed) || self.diagnosed.load(Ordering::Relaxed) {
            ForwardHealth::Failing
        } else {
            ForwardHealth::Delivering
        }
    }

    /// Re-arm after a connection succeeds, so a relapse is explained again.
    fn clear(&self) {
        self.observed.store(true, Ordering::Relaxed);
        self.reported.store(false, Ordering::Relaxed);
        self.diagnosed.store(false, Ordering::Relaxed);
    }

    /// Claim the right to report this failure, latching what it reports.
    ///
    /// A broken path latches separately from ordinary failures, so a refusal
    /// while a service is still starting cannot swallow the explanation of a
    /// path that later stops working.
    fn next_report(&self, path: DialPath, kind: io::ErrorKind) -> DialReport {
        self.observed.store(true, Ordering::Relaxed);
        if breaks_path(path, kind) {
            if self.diagnosed.swap(true, Ordering::Relaxed) {
                return DialReport::Repeat;
            }
            self.reported.store(true, Ordering::Relaxed);
            return DialReport::Diagnose;
        }
        if self.reported.swap(true, Ordering::Relaxed) {
            return DialReport::Repeat;
        }
        DialReport::Failure
    }
}

/// Whether an error kind means the path itself is broken, rather than the
/// service behind it being absent.
///
/// A direct dial adds `TimedOut` to what the reachability probe treats as
/// unreachable, because the probe bounds its own connect while this one does
/// not: a connection dropped silently by host policy surfaces here only as the
/// operating system's own timeout.
const fn breaks_path(path: DialPath, kind: io::ErrorKind) -> bool {
    match path {
        DialPath::Direct => {
            crate::reachability::is_unreachable(kind) || matches!(kind, io::ErrorKind::TimedOut)
        }
        DialPath::Tunnel => matches!(
            kind,
            io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe | io::ErrorKind::TimedOut
        ),
    }
}

/// Describe a failed upstream dial, collapsing repeats for the same proxy.
///
/// A broken path is latched separately from ordinary failures. A refused
/// connection while a dev server is still starting is the common case, and
/// latching on it would otherwise swallow the explanation of a path that later
/// stops working altogether.
fn report_dial_failure(
    state: &DialFailureState,
    path: DialPath,
    target: &str,
    port: u16,
    e: &io::Error,
) {
    match state.next_report(path, e.kind()) {
        DialReport::Diagnose => match path {
            DialPath::Direct => warn!(
                "Proxy connect to {target}:{port} failed: {e}. This host cannot open a \
                 connection to the container, so every forwarded port for it will fail the \
                 same way. Check that the container runtime still routes to it, and that no \
                 firewall or network policy blocks the cella daemon specifically."
            ),
            DialPath::Tunnel => warn!(
                "Forward to {target}:{port} failed: {e}. The agent tunnel could not carry \
                 the connection, so forwards for that container will fail the same way until \
                 its agent reconnects."
            ),
        },
        DialReport::Failure => warn!("Proxy connect to {target}:{port} failed: {e}"),
        DialReport::Repeat => debug!("Proxy connect to {target}:{port} failed again: {e}"),
    }
}

/// Start a direct-IP TCP proxy from `host_port` to the given `IP:port`.
async fn start_direct_proxy(
    host_port: u16,
    ip: String,
    port: u16,
) -> Result<ProxyHandle, io::Error> {
    let listener = TcpListener::bind(("127.0.0.1", host_port)).await?;
    debug!("Direct proxy listening on 127.0.0.1:{host_port} -> {ip}:{port}");

    let dial_state = Arc::new(DialFailureState::default());
    let published = Arc::clone(&dial_state);
    let handle = tokio::spawn(async move {
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Proxy accept error on port {host_port}: {e}");
                    continue;
                }
            };

            let ip = ip.clone();
            let dial_state = Arc::clone(&dial_state);
            debug!("Proxy connection from {peer} on port {host_port}");

            tokio::spawn(async move {
                match TcpStream::connect((ip.as_str(), port)).await {
                    Ok(mut outbound) => {
                        dial_state.clear();
                        let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                    Err(e) => {
                        report_dial_failure(&dial_state, DialPath::Direct, &ip, port, &e);
                        let _ = inbound.shutdown().await;
                    }
                }
            });
        }
    });

    Ok(ProxyHandle {
        handle,
        dial_state: published,
    })
}

/// Start an agent-tunnel TCP proxy from `host_port` through the agent.
async fn start_tunnel_proxy(
    host_port: u16,
    container_name: String,
    target_port: u16,
    target_host: Option<String>,
    broker: Arc<TunnelBroker>,
    container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> Result<ProxyHandle, io::Error> {
    let listener = TcpListener::bind(("127.0.0.1", host_port)).await?;
    let host_label = target_host.as_deref().unwrap_or("localhost");
    debug!(
        "Tunnel proxy listening on 127.0.0.1:{host_port} -> agent:{container_name}:{host_label}:{target_port}"
    );

    let tunnel_state = Arc::new(DialFailureState::default());
    let published = Arc::clone(&tunnel_state);
    let handle = tokio::spawn(async move {
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Tunnel proxy accept error on port {host_port}: {e}");
                    continue;
                }
            };

            let broker = broker.clone();
            let handles = container_handles.clone();
            let name = container_name.clone();
            let host = target_host.clone();
            let tunnel_state = Arc::clone(&tunnel_state);
            debug!("Tunnel proxy connection from {peer} on port {host_port}");

            tokio::spawn(async move {
                if let Err(e) = handle_tunnel_proxy_connection(
                    &mut inbound,
                    &name,
                    target_port,
                    host,
                    &broker,
                    &handles,
                )
                .await
                {
                    report_dial_failure(&tunnel_state, DialPath::Tunnel, &name, target_port, &e);
                    let _ = inbound.shutdown().await;
                } else {
                    tunnel_state.clear();
                }
            });
        }
    });

    Ok(ProxyHandle {
        handle,
        dial_state: published,
    })
}

async fn handle_tunnel_proxy_connection(
    inbound: &mut TcpStream,
    container_name: &str,
    target_port: u16,
    target_host: Option<String>,
    broker: &TunnelBroker,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> Result<(), io::Error> {
    let (connection_id, rx) = broker.request_tunnel().await;

    let agent_tx = {
        let handles = container_handles.lock().await;
        handles.get(container_name).and_then(|h| h.agent_tx.clone())
    };

    let Some(agent_tx) = agent_tx else {
        broker.cancel(connection_id).await;
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "agent not connected",
        ));
    };

    if agent_tx
        .send(DaemonMessage::TunnelRequest {
            connection_id,
            target_port,
            target_host,
        })
        .await
        .is_err()
    {
        broker.cancel(connection_id).await;
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "agent channel closed",
        ));
    }

    let tunnel_stream = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;

    let mut tunnel = match tunnel_stream {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => {
            broker.cancel(connection_id).await;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "tunnel delivery failed",
            ));
        }
        Err(_) => {
            broker.cancel(connection_id).await;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tunnel request timed out",
            ));
        }
    };

    let _ = copy_bidirectional(inbound, &mut tunnel).await;
    Ok(())
}

/// Run the proxy coordinator that manages TCP proxy lifecycle.
///
/// Receives `ProxyCommand` messages and starts/stops TCP proxies accordingly.
pub async fn run_proxy_coordinator(
    mut rx: tokio::sync::mpsc::Receiver<ProxyCommand>,
    ctx: Option<ProxyCoordinatorContext>,
    health: ForwardHealthTable,
) {
    let mut proxies: HashMap<u16, ProxyHandle> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            ProxyCommand::Start {
                host_port,
                target,
                result_tx,
            } => {
                let result = match target {
                    ProxyStartTarget::DirectIp { ip, port } => {
                        start_direct_proxy(host_port, ip, port).await
                    }
                    ProxyStartTarget::AgentTunnel {
                        container_name,
                        port,
                        target_host,
                    } => {
                        if let Some(ref ctx) = ctx {
                            start_tunnel_proxy(
                                host_port,
                                container_name,
                                port,
                                target_host,
                                ctx.tunnel_broker.clone(),
                                ctx.container_handles.clone(),
                            )
                            .await
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "tunnel proxy not available",
                            ))
                        }
                    }
                };
                match result {
                    Ok(handle) => {
                        debug!("Started proxy on localhost:{host_port}");
                        health
                            .lock()
                            .await
                            .insert(host_port, Arc::clone(&handle.dial_state));
                        proxies.insert(host_port, handle);
                        if let Some(tx) = result_tx {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Err(e) => {
                        warn!("Failed to start proxy on port {host_port}: {e}");
                        if let Some(tx) = result_tx {
                            let _ = tx.send(Err(e));
                        }
                    }
                }
            }
            ProxyCommand::Stop { host_port } => {
                health.lock().await.remove(&host_port);
                if let Some(handle) = proxies.remove(&host_port) {
                    handle.abort();
                    debug!("Stopped proxy on port {host_port}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_proxy_starts_and_stops() {
        let handle = start_direct_proxy(0, "127.0.0.1".to_string(), 9999)
            .await
            .unwrap();
        handle.abort();
    }

    // -- ProxyStartTarget --

    #[test]
    fn proxy_start_target_debug_contains_ip_and_port() {
        let target = ProxyStartTarget::DirectIp {
            ip: "10.0.0.5".into(),
            port: 8080,
        };
        let dbg = format!("{target:?}");
        assert!(dbg.contains("10.0.0.5"));
        assert!(dbg.contains("8080"));
    }

    #[test]
    fn proxy_start_target_clone() {
        let original = ProxyStartTarget::DirectIp {
            ip: "1.2.3.4".into(),
            port: 443,
        };
        let cloned = clone_target(&original);
        assert!(matches!(
            cloned,
            ProxyStartTarget::DirectIp { ref ip, port: 443 } if ip == "1.2.3.4"
        ));
    }

    fn clone_target(t: &ProxyStartTarget) -> ProxyStartTarget {
        t.clone()
    }

    #[test]
    fn proxy_start_target_agent_tunnel_debug() {
        let target = ProxyStartTarget::AgentTunnel {
            container_name: "cella-test".into(),
            port: 3000,
            target_host: None,
        };
        let dbg = format!("{target:?}");
        assert!(dbg.contains("cella-test"));
        assert!(dbg.contains("3000"));
    }

    #[test]
    fn proxy_start_target_agent_tunnel_with_host() {
        let target = ProxyStartTarget::AgentTunnel {
            container_name: "cella-test".into(),
            port: 5432,
            target_host: Some("db".to_string()),
        };
        let dbg = format!("{target:?}");
        assert!(dbg.contains("cella-test"));
        assert!(dbg.contains("5432"));
        assert!(dbg.contains("db"));
        // target_host is carried through clone correctly.
        assert!(matches!(
            target,
            ProxyStartTarget::AgentTunnel {
                ref target_host,
                port: 5432,
                ..
            } if target_host.as_deref() == Some("db")
        ));
    }

    // -- ProxyCommand construction --

    #[test]
    fn proxy_command_start_fields() {
        let cmd = ProxyCommand::Start {
            host_port: 3000,
            target: ProxyStartTarget::DirectIp {
                ip: "172.17.0.2".into(),
                port: 8080,
            },
            result_tx: None,
        };
        match cmd {
            ProxyCommand::Start {
                host_port, target, ..
            } => {
                assert_eq!(host_port, 3000);
                assert!(matches!(
                    target,
                    ProxyStartTarget::DirectIp { ref ip, port: 8080 } if ip == "172.17.0.2"
                ));
            }
            ProxyCommand::Stop { .. } => panic!("expected Start"),
        }
    }

    #[test]
    fn proxy_command_stop_fields() {
        let cmd = ProxyCommand::Stop { host_port: 5000 };
        match cmd {
            ProxyCommand::Stop { host_port } => assert_eq!(host_port, 5000),
            ProxyCommand::Start { .. } => panic!("expected Stop"),
        }
    }

    // -- ProxyHandle::abort --

    #[tokio::test]
    async fn proxy_handle_abort_is_idempotent() {
        let handle = start_direct_proxy(0, "127.0.0.1".into(), 1).await.unwrap();
        handle.abort();
    }

    // -- start_direct_proxy binding --

    #[tokio::test]
    async fn start_direct_proxy_port_zero_binds_random() {
        let handle = start_direct_proxy(0, "127.0.0.1".into(), 1234)
            .await
            .unwrap();
        handle.abort();
    }

    // -- report_dial_failure --

    #[test]
    fn health_starts_unknown_and_follows_the_last_outcome() {
        let state = DialFailureState::default();
        assert_eq!(
            state.health(),
            ForwardHealth::Unknown,
            "a forward nothing has used yet has nothing to report"
        );

        let err = io::Error::from(io::ErrorKind::HostUnreachable);
        report_dial_failure(&state, DialPath::Direct, "192.168.97.3", 37479, &err);
        assert_eq!(state.health(), ForwardHealth::Failing);

        state.clear();
        assert_eq!(state.health(), ForwardHealth::Delivering);
    }

    #[test]
    fn a_broken_path_is_diagnosed_once() {
        let state = DialFailureState::default();
        let err = io::Error::from(io::ErrorKind::HostUnreachable);

        report_dial_failure(&state, DialPath::Direct, "192.168.97.3", 37479, &err);
        assert!(state.diagnosed.load(Ordering::Relaxed));

        // Repeats stay latched, so the explanation is not re-emitted per connection.
        report_dial_failure(&state, DialPath::Direct, "192.168.97.3", 37479, &err);
        assert!(state.diagnosed.load(Ordering::Relaxed));
    }

    #[test]
    fn a_refused_connection_does_not_swallow_a_later_diagnosis() {
        let state = DialFailureState::default();

        // The everyday case: the workspace's server has not bound yet.
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        report_dial_failure(&state, DialPath::Direct, "192.168.97.3", 37479, &refused);
        assert!(state.reported.load(Ordering::Relaxed));
        assert!(
            !state.diagnosed.load(Ordering::Relaxed),
            "a refusal says nothing about the path and must not latch the diagnosis"
        );

        // The path then breaks: the explanation must still be emitted.
        let unreachable = io::Error::from(io::ErrorKind::HostUnreachable);
        report_dial_failure(
            &state,
            DialPath::Direct,
            "192.168.97.3",
            37479,
            &unreachable,
        );
        assert!(state.diagnosed.load(Ordering::Relaxed));
    }

    #[test]
    fn a_success_re_arms_both_latches() {
        let state = DialFailureState::default();
        let err = io::Error::from(io::ErrorKind::HostUnreachable);

        report_dial_failure(&state, DialPath::Direct, "192.168.97.3", 37479, &err);
        state.clear();

        assert!(!state.reported.load(Ordering::Relaxed));
        assert!(!state.diagnosed.load(Ordering::Relaxed));
    }

    #[test]
    fn each_path_diagnoses_its_own_failure_kinds() {
        // A timeout breaks either path.
        assert!(breaks_path(DialPath::Direct, io::ErrorKind::TimedOut));
        assert!(breaks_path(DialPath::Tunnel, io::ErrorKind::TimedOut));

        // A missing agent breaks the tunnel, and says nothing about a direct dial.
        assert!(breaks_path(DialPath::Tunnel, io::ErrorKind::NotConnected));
        assert!(!breaks_path(DialPath::Direct, io::ErrorKind::NotConnected));

        // An unroutable host breaks a direct dial; the tunnel never dials it.
        assert!(breaks_path(
            DialPath::Direct,
            io::ErrorKind::HostUnreachable
        ));
        assert!(!breaks_path(
            DialPath::Tunnel,
            io::ErrorKind::HostUnreachable
        ));

        // A refusal means the path worked and nothing was listening.
        assert!(!breaks_path(
            DialPath::Direct,
            io::ErrorKind::ConnectionRefused
        ));
        assert!(!breaks_path(
            DialPath::Tunnel,
            io::ErrorKind::ConnectionRefused
        ));
    }

    // -- run_proxy_coordinator --

    #[tokio::test]
    async fn coordinator_stop_unknown_port_is_harmless() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let coordinator = tokio::spawn(run_proxy_coordinator(rx, None, new_health_table()));

        tx.send(ProxyCommand::Stop { host_port: 9999 })
            .await
            .unwrap();
        drop(tx);
        coordinator.await.unwrap();
    }

    #[tokio::test]
    async fn coordinator_start_and_stop_lifecycle() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let coordinator = tokio::spawn(run_proxy_coordinator(rx, None, new_health_table()));

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tx.send(ProxyCommand::Start {
            host_port: 0,
            target: ProxyStartTarget::DirectIp {
                ip: "127.0.0.1".into(),
                port: 1,
            },
            result_tx: Some(result_tx),
        })
        .await
        .unwrap();

        result_rx.await.unwrap().unwrap();

        drop(tx);
        coordinator.await.unwrap();
    }
}
