//! Localhost-to-all-interfaces TCP proxy.
//!
//! When an app inside the container binds only to localhost (127.0.0.1 or
//! `[::1]`), this proxy binds a random port on 0.0.0.0 and forwards
//! connections to localhost. The daemon uses this proxy port to reach the
//! app through the container's external interface.
//!
//! The proxy uses a **different port** than the app to avoid a self-loop:
//! binding `0.0.0.0:APP_PORT` and connecting to `127.0.0.1:APP_PORT` would
//! connect back to the proxy itself (since `0.0.0.0` includes `127.0.0.1`).

use std::io;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Start a TCP proxy from a random port on all interfaces to `localhost:app_port`.
///
/// Returns `(proxy_port, task_handle)`. The caller should communicate the
/// proxy port to the daemon and keep the handle to abort the proxy later.
///
/// Connects to `"localhost"` which resolves to both 127.0.0.1 and `[::1]`,
/// supporting apps bound to either IPv4 or IPv6 loopback.
///
/// # Errors
///
/// Returns error if binding fails.
pub async fn proxy_localhost_to_all(app_port: u16) -> Result<(u16, JoinHandle<()>), io::Error> {
    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let proxy_port = listener.local_addr()?.port();
    debug!("Proxy listening on 0.0.0.0:{proxy_port} -> localhost:{app_port}");

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut inbound, peer)) => {
                    debug!("Proxy connection from {peer} to localhost:{app_port}");
                    tokio::spawn(async move {
                        match TcpStream::connect(("localhost", app_port)).await {
                            Ok(mut outbound) => {
                                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                            }
                            Err(e) => {
                                warn!("Proxy connect to localhost:{app_port} failed: {e}");
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!("Proxy accept error for app port {app_port}: {e}");
                }
            }
        }
    });

    Ok((proxy_port, handle))
}
