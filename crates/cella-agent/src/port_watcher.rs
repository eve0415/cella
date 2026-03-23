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

/// Port mappings from daemon (container_port → host_port).
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

    // Start localhost proxy BEFORE reporting to daemon, so the proxy
    // is ready when the daemon starts its host-side proxy.
    let mut agent_proxy_port = None;
    if listener.bind == BindAddress::Localhost && !proxy_handles.contains_key(&listener.port) {
        let port = listener.port;
        match port_proxy::proxy_localhost_to_all(port).await {
            Ok((pp, handle)) => {
                debug!("Agent proxy for port {port} on 0.0.0.0:{pp}");
                agent_proxy_port = Some(pp);
                proxy_ports.insert(port, pp);
                proxy_handles.insert(port, handle);
            }
            Err(e) => {
                warn!("Localhost proxy for port {port} failed: {e}");
            }
        }
    }

    // Send port_open to daemon and read port mapping response
    let msg = AgentMessage::PortOpen {
        port: listener.port,
        protocol: listener.protocol,
        process: process.clone(),
        bind: listener.bind,
        proxy_port: agent_proxy_port,
    };
    let send_result = {
        let mut ctrl = control.lock().await;
        match ctrl.send(&msg).await {
            Ok(()) => Ok(ctrl.recv().await),
            Err(e) => Err(e),
        }
    };

    match send_result {
        Ok(Ok(DaemonMessage::PortMapping {
            container_port,
            host_port,
        })) => {
            debug!("Port mapping: container:{container_port} -> host:{host_port}");
            port_map.lock().await.insert(container_port, host_port);
            write_port_map(port_map).await;
            true
        }
        Ok(Ok(_)) => {
            debug!("Unexpected response to PortOpen (no mapping allocated)");
            true
        }
        Ok(Err(e)) => {
            debug!("No response to PortOpen: {e}");
            true
        }
        Err(e) => {
            warn!("Failed to report port open: {e}");
            false
        }
    }
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

            if !send_ok {
                continue;
            }

            ports_detected.fetch_add(1, Ordering::Relaxed);
            known.insert(key, listener.clone());
        }

        // Detect closed listeners
        let closed_keys: Vec<(u16, PortProtocol)> = known
            .keys()
            .filter(|key| !current.iter().any(|l| (l.port, l.protocol) == **key))
            .copied()
            .collect();

        for key in closed_keys {
            info!("Listener closed: port {} ({:?})", key.0, key.1);
            known.remove(&key);

            // Send port_closed to daemon
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
                    write_port_map(&port_map).await;
                }
            }

            // Stop localhost proxy and clean up proxy port mapping
            if let Some(handle) = proxy_handles.remove(&key.0) {
                handle.abort();
            }
            proxy_ports.remove(&key.0);
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
        let closed: Vec<(u16, PortProtocol)> = known
            .keys()
            .filter(|k| !current.iter().any(|l| (l.port, l.protocol) == **k))
            .copied()
            .collect();

        for key in closed {
            known.remove(&key);
            if let Some(handle) = proxy_handles.remove(&key.0) {
                handle.abort();
            }
        }
    }
}
