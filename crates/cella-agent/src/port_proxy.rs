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

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn proxy_binds_to_random_port() {
        let app_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app_listener.local_addr().unwrap().port();

        let (proxy_port, handle) = proxy_localhost_to_all(app_port).await.unwrap();

        assert_ne!(proxy_port, 0);
        assert_ne!(proxy_port, app_port);

        handle.abort();
    }

    #[tokio::test]
    async fn proxy_forwards_data_to_app() {
        let app_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app_listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = app_listener.accept().await {
                let mut buf = vec![0u8; 1024];
                let n = AsyncReadExt::read(&mut stream, &mut buf).await.unwrap_or(0);
                let _ = AsyncWriteExt::write_all(&mut stream, &buf[..n]).await;
            }
        });

        let (proxy_port, handle) = proxy_localhost_to_all(app_port).await.unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        let test_data = b"hello from test";
        AsyncWriteExt::write_all(&mut stream, test_data)
            .await
            .unwrap();
        AsyncWriteExt::shutdown(&mut stream).await.unwrap();

        let mut response = Vec::new();
        AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .unwrap();
        assert_eq!(response, test_data);

        handle.abort();
    }

    #[tokio::test]
    async fn proxy_returns_valid_port_range() {
        let app_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app_listener.local_addr().unwrap().port();

        let (proxy_port, handle) = proxy_localhost_to_all(app_port).await.unwrap();
        assert!(proxy_port > 0);

        handle.abort();
    }

    #[tokio::test]
    async fn proxy_handles_connection_to_closed_app() {
        let tmp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = tmp_listener.local_addr().unwrap().port();
        drop(tmp_listener);

        let (proxy_port, handle) = proxy_localhost_to_all(dead_port).await.unwrap();

        let stream = TcpStream::connect(("127.0.0.1", proxy_port)).await;
        assert!(stream.is_ok());

        let mut stream = stream.unwrap();
        let mut buf = vec![0u8; 1024];
        let result = AsyncReadExt::read(&mut stream, &mut buf).await;
        match result {
            Ok(0) | Err(_) => {}
            Ok(_) => panic!("expected connection to be closed"),
        }

        handle.abort();
    }
}
