//! HTTP reverse proxy server with hostname-based routing.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error_page;
use crate::hostname::parse_hostname;
use crate::router::{BackendTarget, ProxyMode, RouteKey, RouteTable};

/// Shared state for the proxy server.
pub type SharedRouteTable = Arc<RwLock<RouteTable>>;
type ProxyBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Start the HTTP reverse proxy server.
///
/// Binds to the given address and routes requests based on the `Host` header.
///
/// # Errors
///
/// Returns `Err` if binding fails (e.g., port 80 already in use).
pub async fn start_proxy_server(
    addr: SocketAddr,
    route_table: SharedRouteTable,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = TcpListener::bind(addr).await?;
    info!("Hostname proxy listening on {addr}");

    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Proxy accept error: {e}");
                    continue;
                }
            };

            let rt = route_table.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, peer, rt).await {
                    debug!("Connection error from {peer}: {e}");
                }
            });
        }
    });

    Ok(handle)
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    route_table: SharedRouteTable,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let io = TokioIo::new(stream);

    hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .http1()
        .keep_alive(true)
        .serve_connection_with_upgrades(
            io,
            service_fn(move |req| {
                let rt = route_table.clone();
                async move { Ok::<_, Infallible>(handle_request(req, peer, &rt).await) }
            }),
        )
        .await?;

    Ok(())
}

async fn handle_request(
    req: Request<Incoming>,
    peer: SocketAddr,
    route_table: &SharedRouteTable,
) -> Response<ProxyBody> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let Some(parsed) = parse_hostname(&host) else {
        let rt = route_table.read().await;
        return html_response(
            StatusCode::NOT_FOUND,
            error_page::no_route_found(&host, &rt),
        );
    };

    let rt = route_table.read().await;
    let target = if let Some(port) = parsed.port {
        rt.lookup(&parsed.project, &parsed.branch, port)
    } else {
        rt.lookup_default(&parsed.project, &parsed.branch)
    };

    let target = match target {
        Some(t) => t.clone(),
        None => {
            return html_response(
                StatusCode::NOT_FOUND,
                error_page::no_route_found(&host, &rt),
            );
        }
    };
    drop(rt);

    let is_ws = is_websocket_upgrade_headers(req.headers());
    proxy_request(req, peer, &host, &target, is_ws).await
}

async fn proxy_request(
    mut req: Request<Incoming>,
    peer: SocketAddr,
    original_host: &str,
    target: &BackendTarget,
    is_websocket: bool,
) -> Response<ProxyBody> {
    let client_upgrade = is_websocket.then(|| hyper::upgrade::on(&mut req));
    let backend_addr = match &target.mode {
        ProxyMode::Localhost => format!("127.0.0.1:{}", target.target_port),
        ProxyMode::DirectIp(ip) => format!("{ip}:{}", target.target_port),
        ProxyMode::AgentTunnel(_) => {
            return unavailable_response(target);
        }
    };

    let backend_stream = match TcpStream::connect(&backend_addr).await {
        Ok(s) => s,
        Err(e) => {
            debug!("Backend connect to {backend_addr} failed: {e}");
            return unavailable_response(target);
        }
    };

    let io = TokioIo::new(backend_stream);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
        Ok(parts) => parts,
        Err(e) => {
            warn!("Backend handshake failed: {e}");
            return error_response(StatusCode::BAD_GATEWAY, "Backend handshake failed");
        }
    };

    if is_websocket {
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                debug!("WebSocket backend connection error: {e}");
            }
        });
    } else {
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Backend connection error: {e}");
            }
        });
    }

    let proxied_req = build_proxied_request(req, original_host, peer, is_websocket);
    match sender.send_request(proxied_req).await {
        Ok(mut resp) => {
            if is_websocket && resp.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(client_upgrade) = client_upgrade {
                    let upstream_upgrade = hyper::upgrade::on(&mut resp);
                    let host = original_host.to_string();
                    tokio::spawn(async move {
                        let (client_io, upstream_io) =
                            match tokio::join!(client_upgrade, upstream_upgrade) {
                                (Ok(client), Ok(upstream)) => (client, upstream),
                                (Err(e), _) | (_, Err(e)) => {
                                    debug!("WebSocket upgrade for {host} failed: {e}");
                                    return;
                                }
                            };
                        let mut client_io = TokioIo::new(client_io);
                        let mut upstream_io = TokioIo::new(upstream_io);
                        if let Err(e) =
                            tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await
                        {
                            debug!("WebSocket stream for {host} ended: {e}");
                        }
                    });
                }
            }
            resp.map(|body| body.map_err(Into::into).boxed())
        }
        Err(e) => {
            debug!("Backend request failed: {e}");
            error_response(StatusCode::BAD_GATEWAY, "Backend request failed")
        }
    }
}

/// Hop-by-hop headers that must not be forwarded (except during upgrades).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

fn build_proxied_request(
    mut req: Request<Incoming>,
    original_host: &str,
    peer: SocketAddr,
    is_websocket: bool,
) -> Request<Incoming> {
    let headers = req.headers_mut();

    if !is_websocket {
        for &hop in HOP_BY_HOP {
            headers.remove(hop);
        }
    }

    if let Ok(v) = HeaderValue::from_str(original_host) {
        headers.insert("x-forwarded-host", v);
    }
    headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    if let Ok(v) = HeaderValue::from_str(&peer.ip().to_string()) {
        headers.insert("x-forwarded-for", v);
    }

    req
}

fn is_websocket_upgrade_headers(headers: &hyper::HeaderMap) -> bool {
    let has_upgrade = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("upgrade"));

    let is_websocket = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    has_upgrade && is_websocket
}

fn unavailable_response(target: &BackendTarget) -> Response<ProxyBody> {
    html_response(
        StatusCode::BAD_GATEWAY,
        error_page::backend_unreachable(
            &RouteKey {
                project: String::new(),
                branch: String::new(),
                port: target.target_port,
            },
            target,
        ),
    )
}

fn html_response(status: StatusCode, body: String) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(full(body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(full("Internal error".to_string()))
                .expect("static response")
        })
}

fn error_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(full(message.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(full("Internal error".to_string()))
                .expect("static response")
        })
}

fn full(data: String) -> ProxyBody {
    Full::new(Bytes::from(data))
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn websocket_upgrade_detection() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("Connection", HeaderValue::from_static("Upgrade"));
        headers.insert("Upgrade", HeaderValue::from_static("websocket"));
        assert!(is_websocket_upgrade_headers(&headers));
    }

    #[test]
    fn non_websocket_not_detected() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("Connection", HeaderValue::from_static("keep-alive"));
        assert!(!is_websocket_upgrade_headers(&headers));
    }

    #[test]
    fn websocket_detection_case_insensitive() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("Connection", HeaderValue::from_static("upgrade"));
        headers.insert("Upgrade", HeaderValue::from_static("WebSocket"));
        assert!(is_websocket_upgrade_headers(&headers));
    }

    #[test]
    fn empty_headers_no_websocket() {
        let headers = hyper::HeaderMap::new();
        assert!(!is_websocket_upgrade_headers(&headers));
    }

    #[test]
    fn html_response_sets_content_type() {
        let resp = html_response(StatusCode::NOT_FOUND, "<h1>test</h1>".to_string());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn error_response_sets_content_type() {
        let resp = error_response(StatusCode::BAD_GATEWAY, "fail");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
    }

    #[test]
    fn unavailable_response_is_502() {
        let target = BackendTarget {
            container_id: "c1".to_string(),
            container_name: "cella-test".to_string(),
            target_port: 3000,
            mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        };
        let resp = unavailable_response(&target);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn proxy_server_binds_and_responds() {
        let rt = Arc::new(RwLock::new(RouteTable::new()));
        let handle = start_proxy_server("127.0.0.1:0".parse().unwrap(), rt)
            .await
            .unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn proxy_server_returns_404_for_unknown_host() {
        let rt = Arc::new(RwLock::new(RouteTable::new()));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        let rt_clone = rt.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt_clone).await;
        });

        let stream = TcpStream::connect(bound_addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);

        let req = Request::builder()
            .header("Host", "3000.main.myapp.localhost")
            .body(Full::<Bytes>::default())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        server.abort();
    }

    #[tokio::test]
    async fn proxy_server_returns_502_for_unreachable_backend() {
        let mut table = RouteTable::new();
        table.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp".to_string(),
                target_port: 19999,
                mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            },
        );
        let rt: SharedRouteTable = Arc::new(RwLock::new(table));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        let rt_clone = rt.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt_clone).await;
        });

        let stream = TcpStream::connect(bound_addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);

        let req = Request::builder()
            .header("Host", "3000.main.myapp.localhost")
            .body(Full::<Bytes>::default())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        server.abort();
    }

    #[tokio::test]
    async fn proxy_forwards_to_real_backend() {
        // Start a simple backend server
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            let (stream, _) = backend_listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(|_req| async {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from("hello from backend")))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });

        let mut table = RouteTable::new();
        table.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp".to_string(),
                target_port: backend_addr.port(),
                mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            },
        );
        let rt: SharedRouteTable = Arc::new(RwLock::new(table));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let rt_clone = rt.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = proxy_listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt_clone).await;
        });

        let stream = TcpStream::connect(proxy_addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);

        let req = Request::builder()
            .header("Host", "3000.main.myapp.localhost")
            .body(Full::<Bytes>::default())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello from backend");

        server.abort();
        backend.abort();
    }

    #[tokio::test]
    async fn proxy_forwards_to_existing_localhost_host_port() {
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            let (stream, _) = backend_listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(|_req| async {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from("via host port")))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });

        let mut table = RouteTable::new();
        table.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp".to_string(),
                target_port: backend_addr.port(),
                mode: ProxyMode::Localhost,
            },
        );
        let rt: SharedRouteTable = Arc::new(RwLock::new(table));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, peer) = proxy_listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt).await;
        });

        let stream = TcpStream::connect(proxy_addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);

        let req = Request::builder()
            .header("Host", "3000.main.myapp.localhost")
            .body(Full::<Bytes>::default())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"via host port");

        server.abort();
        backend.abort();
    }

    #[tokio::test]
    async fn proxy_streams_response_body_without_buffering() {
        use futures_util::StreamExt;
        use http_body_util::StreamBody;
        use hyper::body::Frame;
        use tokio::sync::oneshot;

        let (release_tx, release_rx) = oneshot::channel::<()>();
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            let (stream, _) = backend_listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let release_rx = std::sync::Mutex::new(Some(release_rx));
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |_req| {
                        let rx = release_rx.lock().unwrap().take().unwrap();
                        async move {
                            let first = futures_util::stream::once(async {
                                Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"first")))
                            });
                            let second = futures_util::stream::once(async move {
                                let _ = rx.await;
                                Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"second")))
                            });
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(200)
                                    .body(StreamBody::new(first.chain(second)))
                                    .unwrap(),
                            )
                        }
                    }),
                )
                .await
                .unwrap();
        });

        let mut table = RouteTable::new();
        table.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp".to_string(),
                target_port: backend_addr.port(),
                mode: ProxyMode::Localhost,
            },
        );
        let rt: SharedRouteTable = Arc::new(RwLock::new(table));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, peer) = proxy_listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt).await;
        });

        let stream = TcpStream::connect(proxy_addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);

        let req = Request::builder()
            .header("Host", "3000.main.myapp.localhost")
            .body(Full::<Bytes>::default())
            .unwrap();
        let mut resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let first = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            resp.body_mut().frame(),
        )
        .await
        .expect("first body chunk should arrive before backend finishes")
        .expect("body should have a frame")
        .unwrap()
        .into_data()
        .unwrap();
        assert_eq!(&first[..], b"first");

        release_tx.send(()).unwrap();
        server.abort();
        backend.abort();
    }

    #[tokio::test]
    async fn proxy_forwards_websocket_upgrade_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            let (mut stream, _) = backend_listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut buf).await.unwrap();
                request.push(buf[0]);
            }
            assert!(
                String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains("upgrade: websocket")
            );
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\n\
                      Connection: Upgrade\r\n\
                      Upgrade: websocket\r\n\
                      \r\n",
                )
                .await
                .unwrap();
            let mut payload = [0u8; 4];
            stream.read_exact(&mut payload).await.unwrap();
            stream.write_all(&payload).await.unwrap();
        });

        let mut table = RouteTable::new();
        table.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp".to_string(),
                target_port: backend_addr.port(),
                mode: ProxyMode::Localhost,
            },
        );
        let rt: SharedRouteTable = Arc::new(RwLock::new(table));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, peer) = proxy_listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt).await;
        });

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                b"GET /socket HTTP/1.1\r\n\
                  Host: 3000.main.myapp.localhost\r\n\
                  Connection: Upgrade\r\n\
                  Upgrade: websocket\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  \r\n",
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        let mut b = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut b).await.unwrap();
            response.push(b[0]);
        }
        assert!(String::from_utf8_lossy(&response).contains("101 Switching Protocols"));

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.read_exact(&mut echoed),
        )
        .await
        .expect("upgraded bytes should be forwarded")
        .unwrap();
        assert_eq!(&echoed, b"ping");

        server.abort();
        backend.abort();
    }

    #[tokio::test]
    async fn proxy_sets_forwarding_headers() {
        use tokio::sync::oneshot;

        let (headers_tx, headers_rx) = oneshot::channel::<hyper::HeaderMap>();
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();

        let backend = tokio::spawn(async move {
            let (stream, _) = backend_listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let tx = std::sync::Mutex::new(Some(headers_tx));
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req: Request<Incoming>| {
                        let captured_tx = tx.lock().unwrap().take();
                        if let Some(tx) = captured_tx {
                            let _ = tx.send(req.headers().clone());
                        }
                        async {
                            Ok::<_, Infallible>(
                                Response::builder().body(Full::new(Bytes::new())).unwrap(),
                            )
                        }
                    }),
                )
                .await
                .unwrap();
        });

        let mut table = RouteTable::new();
        table.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp".to_string(),
                target_port: backend_addr.port(),
                mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            },
        );
        let rt: SharedRouteTable = Arc::new(RwLock::new(table));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let rt_clone = rt.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = proxy_listener.accept().await.unwrap();
            let _ = handle_connection(stream, peer, rt_clone).await;
        });

        let stream = TcpStream::connect(proxy_addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);

        let req = Request::builder()
            .header("Host", "3000.main.myapp.localhost")
            .body(Full::<Bytes>::default())
            .unwrap();
        let _resp = sender.send_request(req).await.unwrap();

        let received_headers = headers_rx.await.unwrap();
        assert_eq!(
            received_headers.get("x-forwarded-host").unwrap(),
            "3000.main.myapp.localhost"
        );
        assert_eq!(received_headers.get("x-forwarded-proto").unwrap(), "http");
        assert!(received_headers.get("x-forwarded-for").is_some());

        server.abort();
        backend.abort();
    }
}
