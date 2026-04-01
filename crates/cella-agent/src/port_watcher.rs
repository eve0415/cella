//! Port watcher: polls /proc/net/tcp for new/closed listeners.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cella_port::detection::{DetectedListener, scan_listeners};
use cella_port::protocol::{AgentMessage, BindAddress, DaemonMessage, PortProtocol};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::port_proxy;
use crate::reconnecting_client::ReconnectingClient;

/// Port mappings from daemon (`container_port` → `host_port`).
pub type PortMap = Arc<Mutex<HashMap<u16, u16>>>;

/// Path where port mappings are written as JSON for child processes.
const PORT_MAP_PATH: &str = "/tmp/cella-port-map";

/// Write the current port map to disk as JSON.
async fn write_port_map(port_map: &PortMap) {
    let map = port_map.lock().await;
    if map.is_empty() {
        let _ = tokio::fs::remove_file(PORT_MAP_PATH).await;
        return;
    }
    match serde_json::to_string(&*map) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(PORT_MAP_PATH, json).await {
                warn!("Failed to write port map: {e}");
            }
        }
        Err(e) => warn!("Failed to serialize port map: {e}"),
    }
}

/// Re-report all known listeners after a reconnect so the daemon has an accurate picture.
async fn re_report_known_listeners(
    control: &Mutex<ReconnectingClient>,
    known: &HashMap<(u16, PortProtocol), DetectedListener>,
    proxy_ports: &HashMap<u16, u16>,
) {
    let mut ctrl = control.lock().await;
    if !ctrl.take_reconnected() {
        return;
    }
    info!(
        "Reconnected to daemon, re-reporting {} known listeners",
        known.len()
    );
    for listener in known.values() {
        let process = cella_port::detection::process_name_for_inode(listener.inode);
        let msg = AgentMessage::PortOpen {
            port: listener.port,
            protocol: listener.protocol,
            process,
            bind: listener.bind,
            proxy_port: proxy_ports.get(&listener.port).copied(),
        };
        if let Err(e) = ctrl.send(&msg).await {
            warn!("Failed to re-report port {}: {e}", listener.port);
        }
    }
}

/// Start a localhost proxy for a listener bound to localhost only.
///
/// Returns the proxy port if a proxy was started, or `None` if not needed.
async fn maybe_start_localhost_proxy(
    listener: &DetectedListener,
    proxy_handles: &mut HashMap<u16, tokio::task::JoinHandle<()>>,
    proxy_ports: &mut HashMap<u16, u16>,
) -> Option<u16> {
    if listener.bind != BindAddress::Localhost || proxy_handles.contains_key(&listener.port) {
        return None;
    }

    let port = listener.port;
    match port_proxy::proxy_localhost_to_all(port).await {
        Ok((pp, handle)) => {
            debug!("Agent proxy for port {port} on 0.0.0.0:{pp}");
            proxy_ports.insert(port, pp);
            proxy_handles.insert(port, handle);
            Some(pp)
        }
        Err(e) => {
            warn!("Localhost proxy for port {port} failed: {e}");
            None
        }
    }
}

/// Record a port mapping received from the daemon.
async fn record_port_mapping(port_map: &PortMap, container_port: u16, host_port: u16) {
    debug!("Port mapping: container:{container_port} -> host:{host_port}");
    port_map.lock().await.insert(container_port, host_port);
    write_port_map(port_map).await;
}

/// Process the daemon's response to a `PortOpen` message.
async fn process_port_open_response(
    response: Result<DaemonMessage, cella_port::CellaPortError>,
    port_map: &PortMap,
) {
    match response {
        Ok(DaemonMessage::PortMapping {
            container_port,
            host_port,
        }) => {
            record_port_mapping(port_map, container_port, host_port).await;
        }
        Ok(_) => {
            debug!("Unexpected response to PortOpen (no mapping allocated)");
        }
        Err(e) => {
            debug!("No response to PortOpen: {e}");
        }
    }
}

/// Send a `PortOpen` message to the daemon and process the port mapping response.
async fn send_port_open_and_record(
    listener: &DetectedListener,
    process: Option<String>,
    agent_proxy_port: Option<u16>,
    control: &Mutex<ReconnectingClient>,
    port_map: &PortMap,
) -> bool {
    let msg = AgentMessage::PortOpen {
        port: listener.port,
        protocol: listener.protocol,
        process,
        bind: listener.bind,
        proxy_port: agent_proxy_port,
    };

    let mut ctrl = control.lock().await;
    if let Err(e) = ctrl.send(&msg).await {
        warn!("Failed to report port open: {e}");
        return false;
    }

    let response = ctrl.recv().await;
    drop(ctrl);
    process_port_open_response(response, port_map).await;
    true
}

/// Handle a single newly-detected listener: start proxy if needed, report to daemon,
/// and read the port mapping response.
///
/// Returns `true` if the listener was successfully reported and should be tracked.
async fn handle_new_listener(
    listener: &DetectedListener,
    control: &Mutex<ReconnectingClient>,
    port_map: &PortMap,
    proxy_handles: &mut HashMap<u16, tokio::task::JoinHandle<()>>,
    proxy_ports: &mut HashMap<u16, u16>,
) -> bool {
    let process = cella_port::detection::process_name_for_inode(listener.inode);
    info!(
        "New listener detected: port {} ({:?}) process={:?}",
        listener.port, listener.bind, process
    );

    let agent_proxy_port = maybe_start_localhost_proxy(listener, proxy_handles, proxy_ports).await;

    send_port_open_and_record(listener, process, agent_proxy_port, control, port_map).await
}

/// Handle a single closed listener: report to daemon and clean up proxies.
async fn handle_closed_listener(
    key: (u16, PortProtocol),
    control: &Mutex<ReconnectingClient>,
    ports_detected: &AtomicUsize,
    port_map: &PortMap,
    proxy_handles: &mut HashMap<u16, tokio::task::JoinHandle<()>>,
    proxy_ports: &mut HashMap<u16, u16>,
) {
    info!("Listener closed: port {} ({:?})", key.0, key.1);

    let msg = AgentMessage::PortClosed {
        port: key.0,
        protocol: key.1,
    };
    {
        let mut ctrl = control.lock().await;
        if let Err(e) = ctrl.send(&msg).await {
            warn!("Failed to report port closed: {e}");
        } else {
            ports_detected.fetch_sub(1, Ordering::Relaxed);
            port_map.lock().await.remove(&key.0);
            write_port_map(port_map).await;
        }
    }

    if let Some(handle) = proxy_handles.remove(&key.0) {
        handle.abort();
    }
    proxy_ports.remove(&key.0);
}

/// Collect keys from `known` that are no longer present in `current` listeners.
fn collect_closed_keys(
    known: &HashMap<(u16, PortProtocol), DetectedListener>,
    current: &std::collections::HashSet<DetectedListener>,
) -> Vec<(u16, PortProtocol)> {
    known
        .keys()
        .filter(|key| !current.iter().any(|l| (l.port, l.protocol) == **key))
        .copied()
        .collect()
}

/// Run the port watcher loop with control socket connection.
pub async fn run(
    poll_interval: Duration,
    control: Arc<Mutex<ReconnectingClient>>,
    ports_detected: Arc<AtomicUsize>,
    port_map: PortMap,
) {
    let proc_path = Path::new("/proc");
    let mut known: HashMap<(u16, PortProtocol), DetectedListener> = HashMap::new();
    let mut proxy_handles: HashMap<u16, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut proxy_ports: HashMap<u16, u16> = HashMap::new();

    loop {
        tokio::time::sleep(poll_interval).await;

        re_report_known_listeners(&control, &known, &proxy_ports).await;

        let Ok(current) = scan_listeners(proc_path) else {
            warn!("Failed to scan /proc/net/tcp, retrying");
            continue;
        };

        // Detect new listeners
        for listener in &current {
            let key = (listener.port, listener.protocol);
            if known.contains_key(&key) {
                continue;
            }

            let send_ok = handle_new_listener(
                listener,
                &control,
                &port_map,
                &mut proxy_handles,
                &mut proxy_ports,
            )
            .await;

            if send_ok {
                ports_detected.fetch_add(1, Ordering::Relaxed);
                known.insert(key, listener.clone());
            }
        }

        // Detect closed listeners
        let closed_keys = collect_closed_keys(&known, &current);

        for key in closed_keys {
            known.remove(&key);
            handle_closed_listener(
                key,
                &control,
                &ports_detected,
                &port_map,
                &mut proxy_handles,
                &mut proxy_ports,
            )
            .await;
        }
    }
}

/// Run in standalone mode without control socket (just localhost proxying).
pub async fn run_standalone(poll_interval: Duration) {
    let proc_path = Path::new("/proc");
    let mut known: HashMap<(u16, PortProtocol), DetectedListener> = HashMap::new();
    let mut proxy_handles: HashMap<u16, tokio::task::JoinHandle<()>> = HashMap::new();

    loop {
        tokio::time::sleep(poll_interval).await;

        let Ok(current) = scan_listeners(proc_path) else {
            continue;
        };

        for listener in &current {
            let key = (listener.port, listener.protocol);
            if known.contains_key(&key) {
                continue;
            }

            if listener.bind == BindAddress::Localhost
                && !proxy_handles.contains_key(&listener.port)
            {
                let port = listener.port;
                match port_proxy::proxy_localhost_to_all(port).await {
                    Ok((pp, handle)) => {
                        info!("Standalone: proxying 0.0.0.0:{pp} -> localhost:{port}");
                        proxy_handles.insert(port, handle);
                    }
                    Err(e) => {
                        warn!("Standalone proxy for port {port} failed: {e}");
                    }
                }
            }

            known.insert(key, listener.clone());
        }

        // Clean up closed
        let closed = collect_closed_keys(&known, &current);

        for key in closed {
            known.remove(&key);
            if let Some(handle) = proxy_handles.remove(&key.0) {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cella_port::protocol::BindAddress;
    use std::collections::HashSet;

    fn make_listener(port: u16, protocol: PortProtocol) -> DetectedListener {
        DetectedListener {
            port,
            protocol,
            bind: BindAddress::All,
            inode: u64::from(port),
        }
    }

    #[test]
    fn collect_closed_keys_empty_known() {
        let known = HashMap::new();
        let current = HashSet::new();
        let closed = collect_closed_keys(&known, &current);
        assert!(closed.is_empty());
    }

    #[test]
    fn collect_closed_keys_nothing_closed() {
        let l1 = make_listener(8080, PortProtocol::Tcp);
        let l2 = make_listener(3000, PortProtocol::Tcp);
        let mut known = HashMap::new();
        known.insert((l1.port, l1.protocol), l1.clone());
        known.insert((l2.port, l2.protocol), l2.clone());

        let mut current = HashSet::new();
        current.insert(l1);
        current.insert(l2);

        let closed = collect_closed_keys(&known, &current);
        assert!(closed.is_empty());
    }

    #[test]
    fn collect_closed_keys_one_closed() {
        let l1 = make_listener(8080, PortProtocol::Tcp);
        let l2 = make_listener(3000, PortProtocol::Tcp);
        let mut known = HashMap::new();
        known.insert((l1.port, l1.protocol), l1.clone());
        known.insert((l2.port, l2.protocol), l2);

        // Only l1 remains in current.
        let mut current = HashSet::new();
        current.insert(l1);

        let closed = collect_closed_keys(&known, &current);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0], (3000, PortProtocol::Tcp));
    }

    #[test]
    fn collect_closed_keys_all_closed() {
        let l1 = make_listener(8080, PortProtocol::Tcp);
        let l2 = make_listener(9090, PortProtocol::Tcp);
        let mut known = HashMap::new();
        known.insert((l1.port, l1.protocol), l1);
        known.insert((l2.port, l2.protocol), l2);

        let current = HashSet::new();
        let closed = collect_closed_keys(&known, &current);
        assert_eq!(closed.len(), 2);
        assert!(closed.contains(&(8080, PortProtocol::Tcp)));
        assert!(closed.contains(&(9090, PortProtocol::Tcp)));
    }

    #[test]
    fn collect_closed_keys_different_protocols() {
        let tcp = make_listener(8080, PortProtocol::Tcp);
        let udp = DetectedListener {
            port: 8080,
            protocol: PortProtocol::Udp,
            bind: BindAddress::All,
            inode: 8081,
        };
        let mut known = HashMap::new();
        known.insert((tcp.port, tcp.protocol), tcp.clone());
        known.insert((udp.port, udp.protocol), udp);

        // Only TCP remains, UDP should be closed.
        let mut current = HashSet::new();
        current.insert(tcp);

        let closed = collect_closed_keys(&known, &current);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0], (8080, PortProtocol::Udp));
    }
}
