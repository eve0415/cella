//! Integration tests for proxy-mediated network policy enforcement.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use rcgen::{CertificateParams, DnType, DnValue, IsCa, Issuer, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::forward_proxy::{self, ForwardProxyHandle};
use crate::proxy_config::AgentProxyConfig;

struct UpstreamServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
    /// Number of TCP connections the server accepted.
    connections: Arc<AtomicUsize>,
}

async fn start_upstream_server() -> UpstreamServer {
    start_upstream_server_at("127.0.0.1")
        .await
        .expect("bind upstream server")
}

/// Start the echoing upstream on `ip`, returning the bind error if `ip` is
/// unavailable here.
async fn start_upstream_server_at(ip: &str) -> std::io::Result<UpstreamServer> {
    let listener = TcpListener::bind((ip, 0)).await?;
    let addr = listener.local_addr()?;
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                break;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut headers = Vec::new();
                let mut byte = [0_u8; 1];
                while stream.read_exact(&mut byte).await.is_ok() {
                    headers.push(byte[0]);
                    if headers.ends_with(b"\r\n\r\n") || headers.ends_with(b"\n\n") {
                        break;
                    }
                }

                // Echo the request head so tests can inspect what the proxy
                // forwarded.
                let body = format!("upstream ok\n{}", String::from_utf8_lossy(&headers));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
        }
    });

    Ok(UpstreamServer {
        addr,
        task,
        connections,
    })
}

async fn start_proxy(config_json: &str) -> ForwardProxyHandle {
    let config = AgentProxyConfig::from_json(config_json).expect("parse proxy config");
    forward_proxy::start_forward_proxy(Arc::new(config))
        .await
        .expect("start forward proxy")
}

fn proxy_config_json(mode: &str, rules: &[Value], log_path: &Path) -> String {
    serde_json::json!({
        "listen_port": 0,
        "mode": mode,
        "rules": rules,
        "upstream_proxy": null,
        "ca_cert_pem": null,
        "ca_key_pem": null,
        "log_path": log_path.to_string_lossy().into_owned(),
    })
    .to_string()
}

fn rule(domain: &str, paths: &[&str], action: &str) -> Value {
    if paths.is_empty() {
        serde_json::json!({
            "domain": domain,
            "action": action,
        })
    } else {
        serde_json::json!({
            "domain": domain,
            "paths": paths,
            "action": action,
        })
    }
}

async fn proxy_http_request(
    proxy_addr: SocketAddr,
    upstream_addr: SocketAddr,
    path: &str,
) -> String {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    let target = format!("http://127.0.0.1:{}{path}", upstream_addr.port());
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        upstream_addr.port()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write proxy request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read proxy response");
    String::from_utf8(response).expect("utf8 proxy response")
}

async fn proxy_connect_request(proxy_addr: SocketAddr, port: u16) -> String {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write CONNECT request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read CONNECT response");
    String::from_utf8(response).expect("utf8 CONNECT response")
}

/// Send CONNECT, then tunnel an HTTP GET through the established connection.
/// Returns the CONNECT status line if it fails (e.g. 403), or the tunneled
/// HTTP response body if the tunnel is established.
async fn proxy_connect_and_tunnel(
    proxy_addr: SocketAddr,
    upstream_addr: SocketAddr,
    path: &str,
) -> String {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    let connect_req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        upstream_addr.port(),
        upstream_addr.port(),
    );
    stream
        .write_all(connect_req.as_bytes())
        .await
        .expect("write CONNECT");

    // Read CONNECT response headers.
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let connect_response = String::from_utf8_lossy(&headers).to_string();
    if !connect_response.starts_with("HTTP/1.1 200") {
        return connect_response;
    }

    // Tunnel is open — send plain HTTP through it.
    let http_req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        upstream_addr.port(),
    );
    stream
        .write_all(http_req.as_bytes())
        .await
        .expect("write HTTP through tunnel");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read tunneled response");
    String::from_utf8(response).expect("utf8 tunneled response")
}

#[tokio::test]
async fn denylist_path_rule_blocks_matching_http_request() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json(
        "denylist",
        &[rule("127.0.0.1", &["/blocked/**"], "block")],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let blocked = proxy_http_request(proxy.local_addr, upstream.addr, "/blocked/secret").await;
    assert!(
        blocked.starts_with("HTTP/1.1 403 Forbidden"),
        "blocked response was {blocked:?}"
    );

    let allowed = proxy_http_request(proxy.local_addr, upstream.addr, "/allowed").await;
    assert!(
        allowed.starts_with("HTTP/1.1 200 OK"),
        "allowed response was {allowed:?}"
    );
    assert!(allowed.contains("upstream ok"));

    let log = std::fs::read_to_string(&log_path).expect("blocked-request log");
    assert!(log.contains("BLOCKED\t127.0.0.1\t/blocked/secret"));

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn allowlist_mode_blocks_unmatched_http_request() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json(
        "allowlist",
        &[rule("127.0.0.1", &["/allowed/**"], "allow")],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let allowed = proxy_http_request(proxy.local_addr, upstream.addr, "/allowed/resource").await;
    assert!(
        allowed.starts_with("HTTP/1.1 200 OK"),
        "allowed response was {allowed:?}"
    );

    let blocked = proxy_http_request(proxy.local_addr, upstream.addr, "/other").await;
    assert!(
        blocked.starts_with("HTTP/1.1 403 Forbidden"),
        "blocked response was {blocked:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn connect_domain_rule_is_blocked_before_upstream_connection() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[rule("127.0.0.1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    let blocked = proxy_connect_request(proxy.local_addr, 9).await;
    assert!(
        blocked.starts_with("HTTP/1.1 403 Forbidden"),
        "CONNECT response was {blocked:?}"
    );

    let log = std::fs::read_to_string(&log_path).expect("blocked-request log");
    assert!(log.contains("BLOCKED\t127.0.0.1\t/"));

    proxy.task.abort();
}

#[tokio::test]
async fn connect_with_path_rules_but_no_mitm_allows_through() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    // Path-level rule but no ca_cert_pem/ca_key_pem → no MITM available.
    let config_json = proxy_config_json(
        "denylist",
        &[rule("127.0.0.1", &["/blocked/**"], "block")],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    // CONNECT should succeed (200) and tunnel to upstream, not 403.
    let response =
        proxy_connect_and_tunnel(proxy.local_addr, upstream.addr, "/blocked/secret").await;
    assert!(
        response.contains("upstream ok"),
        "expected tunnel passthrough, got {response:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn connect_domain_block_still_enforced_without_mitm() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    // Domain has both a domain-level block AND a path-level rule.
    // Without MITM, path rules can't fire but the domain block must.
    let config_json = proxy_config_json(
        "denylist",
        &[
            rule("127.0.0.1", &[], "block"),
            rule("127.0.0.1", &["/secret/**"], "block"),
        ],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let blocked = proxy_connect_request(proxy.local_addr, 9).await;
    assert!(
        blocked.starts_with("HTTP/1.1 403 Forbidden"),
        "domain block should still apply without MITM, got {blocked:?}"
    );

    proxy.task.abort();
}

/// Read one HTTP/1.1 response framed by `Content-Length`.
///
/// Returns `None` when the connection closes before a full response arrives.
async fn read_http_response<S: AsyncReadExt + Unpin>(stream: &mut S) -> Option<String> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.ok()?;
        headers.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&headers).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body).await.ok()?;
    Some(head + &String::from_utf8_lossy(&body))
}

/// Send each path as a keep-alive plain-HTTP proxy request on one connection.
async fn proxy_keepalive_requests(
    proxy_addr: SocketAddr,
    upstream_addr: SocketAddr,
    paths: &[&str],
) -> Vec<Option<String>> {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    let port = upstream_addr.port();
    let mut responses = Vec::new();
    for path in paths {
        let request =
            format!("GET http://127.0.0.1:{port}{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        if stream.write_all(request.as_bytes()).await.is_err() {
            responses.push(None);
            continue;
        }
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_http_response(&mut stream),
        )
        .await
        .ok()
        .flatten();
        responses.push(response);
    }
    responses
}

#[tokio::test]
async fn keepalive_plain_http_evaluates_every_request() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json(
        "denylist",
        &[rule("127.0.0.1", &["/blocked/**"], "block")],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let responses =
        proxy_keepalive_requests(proxy.local_addr, upstream.addr, &["/allowed", "/blocked/x"])
            .await;
    let first = responses[0].as_deref().unwrap_or_default();
    assert!(
        first.starts_with("HTTP/1.1 200") && first.contains("upstream ok"),
        "first response was {first:?}"
    );
    let second = responses[1].as_deref().unwrap_or_default();
    assert!(
        second.starts_with("HTTP/1.1 403"),
        "second request on the same connection must be evaluated, got {second:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn keepalive_plain_http_reconnects_after_upstream_close() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    // The upstream answers with `Connection: close`, so the second request
    // must go out on a fresh upstream connection.
    let responses = proxy_keepalive_requests(proxy.local_addr, upstream.addr, &["/a", "/b"]).await;
    for response in &responses {
        let response = response.as_deref().unwrap_or_default();
        assert!(
            response.starts_with("HTTP/1.1 200") && response.contains("upstream ok"),
            "response was {response:?}"
        );
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 2);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_overwrites_host_with_target_authority() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr)
        .await
        .expect("connect to proxy");
    let port = upstream.addr.port();
    let request = format!(
        "GET http://127.0.0.1:{port}/echo HTTP/1.1\r\nHost: elsewhere.invalid\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let response = read_http_response(&mut stream).await.unwrap_or_default();

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "response was {response:?}"
    );
    assert!(
        response.contains("GET /echo HTTP/1.1"),
        "upstream must receive origin-form, got {response:?}"
    );
    let host_lines: Vec<&str> = response
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("host:"))
        .collect();
    assert_eq!(host_lines, [format!("host: 127.0.0.1:{port}")]);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn origin_form_plain_request_is_rejected() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr)
        .await
        .expect("connect to proxy");
    let request = format!(
        "GET /path HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        proxy.local_addr
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_http_response(&mut stream),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "response was {response:?}"
    );

    proxy.task.abort();
}

/// Upstream that keeps connections open and tracks how many are live.
struct KeepAliveUpstream {
    addr: SocketAddr,
    task: JoinHandle<()>,
    accepted: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
}

async fn start_keepalive_upstream() -> KeepAliveUpstream {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind keep-alive upstream");
    let addr = listener.local_addr().expect("keep-alive upstream addr");
    let accepted = Arc::new(AtomicUsize::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let (accepted_in, live_in) = (accepted.clone(), live.clone());

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                break;
            };
            accepted_in.fetch_add(1, Ordering::SeqCst);
            live_in.fetch_add(1, Ordering::SeqCst);
            let live = live_in.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while stream.read_exact(&mut byte).await.is_ok() {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        head.clear();
                        if stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                live.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    KeepAliveUpstream {
        addr,
        task,
        accepted,
        live,
    }
}

/// Send keep-alive GETs for each absolute URL on one proxy connection.
///
/// Returns the still-open client connection with the responses.
async fn proxy_keepalive_urls(
    proxy_addr: SocketAddr,
    urls: &[String],
) -> (TcpStream, Vec<Option<String>>) {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    let mut responses = Vec::new();
    for url in urls {
        let request = format!("GET {url} HTTP/1.1\r\nHost: ignored.invalid\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_http_response(&mut stream),
        )
        .await
        .ok()
        .flatten();
        responses.push(response);
    }
    (stream, responses)
}

#[tokio::test]
async fn plain_http_pool_is_capped_per_client_connection() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let mut upstreams = Vec::new();
    for _ in 0..12 {
        upstreams.push(start_keepalive_upstream().await);
    }
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let urls: Vec<String> = upstreams
        .iter()
        .map(|u| format!("http://127.0.0.1:{}/", u.addr.port()))
        .collect();
    let (client, responses) = proxy_keepalive_urls(proxy.local_addr, &urls).await;
    assert!(
        responses
            .iter()
            .all(|r| r.as_deref().is_some_and(|r| r.starts_with("HTTP/1.1 200")))
    );

    // Evicted senders close their connections asynchronously.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let live = loop {
        let live: usize = upstreams
            .iter()
            .map(|u| u.live.load(Ordering::SeqCst))
            .sum();
        if live <= 8 || tokio::time::Instant::now() >= deadline {
            break live;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(live <= 8, "{live} upstream connections still open");

    drop(client);
    proxy.task.abort();
    for upstream in upstreams {
        upstream.task.abort();
    }
}

#[tokio::test]
async fn plain_http_pool_key_normalizes_port_spelling() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_keepalive_upstream().await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    let urls = [
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:0{port}/b"),
        format!("http://127.0.0.1:00{port}/c"),
    ];
    let (client, responses) = proxy_keepalive_urls(proxy.local_addr, &urls).await;
    assert!(
        responses
            .iter()
            .all(|r| r.as_deref().is_some_and(|r| r.starts_with("HTTP/1.1 200")))
    );
    assert_eq!(upstream.accepted.load(Ordering::SeqCst), 1);

    drop(client);
    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_host_header_drops_userinfo() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr)
        .await
        .expect("connect to proxy");
    let port = upstream.addr.port();
    let request = format!(
        "GET http://user:secret@127.0.0.1:{port}/echo HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let response = read_http_response(&mut stream).await.unwrap_or_default();

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "response was {response:?}"
    );
    assert!(
        !response.contains("secret"),
        "userinfo leaked upstream: {response:?}"
    );
    let host_lines: Vec<&str> = response
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("host:"))
        .collect();
    assert_eq!(host_lines, [format!("host: 127.0.0.1:{port}")]);

    proxy.task.abort();
    upstream.task.abort();
}

/// Send one `Connection: close` request with the raw `head` (request line
/// and header lines, without the final blank line) and read the response.
async fn proxy_raw_request(proxy_addr: SocketAddr, head: &[u8]) -> String {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    stream.write_all(head).await.expect("write head");
    stream
        .write_all(b"Connection: close\r\n\r\n")
        .await
        .expect("write head end");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_http_response(&mut stream),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

#[tokio::test]
async fn plain_http_reaches_ipv6_literal_target() {
    let Ok(upstream) = start_upstream_server_at("::1").await else {
        println!("skipping: cannot bind [::1] in this environment");
        return;
    };
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    let head = format!("GET http://[::1]:{port}/echo HTTP/1.1\r\nHost: x\r\n");
    let response = proxy_raw_request(proxy.local_addr, head.as_bytes()).await;
    assert!(
        response.starts_with("HTTP/1.1 200") && response.contains("upstream ok"),
        "response was {response:?}"
    );
    let host_lines: Vec<&str> = response
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("host:"))
        .collect();
    assert_eq!(host_lines, [format!("host: [::1]:{port}")]);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_rule_matches_unbracketed_ipv6_literal() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[rule("::1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    let response = proxy_raw_request(
        proxy.local_addr,
        b"GET http://[::1]:9/x HTTP/1.1\r\nHost: x\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "response was {response:?}"
    );
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(log.contains("BLOCKED\t::1\t/x"), "log was {log:?}");

    proxy.task.abort();
}

/// Send `target` both as a plain-HTTP request and as a CONNECT, returning
/// both responses.
async fn proxy_plain_and_connect(proxy_addr: SocketAddr, target: &str) -> [String; 2] {
    let plain = format!("GET http://{target}/x HTTP/1.1\r\nHost: x\r\n");
    let connect = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    [
        proxy_raw_request(proxy_addr, plain.as_bytes()).await,
        proxy_raw_request(proxy_addr, connect.as_bytes()).await,
    ]
}

#[tokio::test]
async fn ipv6_literal_spellings_match_canonical_rules() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json(
        "denylist",
        &[rule("127.0.0.1", &[], "block"), rule("::1", &[], "block")],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    for (target, canonical) in [
        (format!("[::ffff:127.0.0.1]:{port}"), "127.0.0.1"),
        (format!("[0:0:0:0:0:0:0:1]:{port}"), "::1"),
    ] {
        for response in proxy_plain_and_connect(proxy.local_addr, &target).await {
            assert!(
                response.starts_with("HTTP/1.1 403"),
                "{target}: response was {response:?}"
            );
        }
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let blocked = format!("BLOCKED\t{canonical}\t");
        assert_eq!(log.matches(&blocked).count(), 2, "log was {log:?}");
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);

    proxy.task.abort();
    upstream.task.abort();
}

/// An IPv4-compatible literal (`::a.b.c.d`) is an IPv6 destination: an IPv4
/// rule does not match it and the proxy does not connect to the embedded
/// IPv4 address.
#[tokio::test]
async fn ipv4_compatible_literal_is_not_taken_as_ipv4() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[rule("127.0.0.1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    // Connecting to `::7f00:1` may hang rather than fail, so send both
    // requests at once and wait for the rule verdicts in the log instead of
    // for the responses.
    let target = format!("[::127.0.0.1]:{}", upstream.addr.port());
    let mut clients = Vec::new();
    for head in [
        format!("GET http://{target}/x HTTP/1.1\r\nHost: x\r\n\r\n"),
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"),
    ] {
        let mut stream = TcpStream::connect(proxy.local_addr)
            .await
            .expect("connect to proxy");
        stream.write_all(head.as_bytes()).await.expect("write head");
        clients.push(stream);
    }
    let mut log = String::new();
    for _ in 0..50 {
        log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.lines().count() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        log.matches("ALLOWED\t::7f00:1\t").count(),
        2,
        "log was {log:?}"
    );
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    drop(clients);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn ipv4_compatible_literal_matches_ipv6_rule() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[rule("::7f00:1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    let target = format!("[::127.0.0.1]:{}", upstream.addr.port());
    for response in proxy_plain_and_connect(proxy.local_addr, &target).await {
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "response was {response:?}"
        );
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert_eq!(
        log.matches("BLOCKED\t::7f00:1\t").count(),
        2,
        "log was {log:?}"
    );
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);

    proxy.task.abort();
    upstream.task.abort();
}

/// Send `host` with the port of an upstream on 127.0.0.1, as a plain-HTTP
/// request and as a CONNECT, with `127.0.0.1` and `::1` blocked, and assert
/// both are rejected with 400 before the rules and without connecting.
async fn assert_unspecified_host_rejected(host: &str) {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json(
        "denylist",
        &[rule("127.0.0.1", &[], "block"), rule("::1", &[], "block")],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let target = format!("{host}:{}", upstream.addr.port());
    let responses = proxy_plain_and_connect(proxy.local_addr, &target).await;
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let connections = upstream.connections.load(Ordering::SeqCst);
    assert!(
        responses.iter().all(|r| r.starts_with("HTTP/1.1 400"))
            && log.is_empty()
            && connections == 0,
        "{target}: responses {responses:?}, log {log:?}, {connections} upstream connections"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn unspecified_ipv6_host_is_rejected() {
    assert_unspecified_host_rejected("[::]").await;
}

#[tokio::test]
async fn unspecified_ipv6_long_form_host_is_rejected() {
    assert_unspecified_host_rejected("[0::0]").await;
}

#[tokio::test]
async fn unspecified_ipv4_mapped_host_is_rejected() {
    assert_unspecified_host_rejected("[::ffff:0.0.0.0]").await;
}

#[tokio::test]
async fn unspecified_ipv4_host_is_rejected() {
    assert_unspecified_host_rejected("0.0.0.0").await;
}

/// Send `head` with an empty host and the port of an upstream on 127.0.0.1,
/// and assert it is rejected with 400 before the rules and without
/// connecting.
async fn assert_empty_host_rejected(head: impl Fn(u16) -> String) {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[rule("127.0.0.1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    let head = head(upstream.addr.port());
    let response = proxy_raw_request(proxy.local_addr, head.as_bytes()).await;
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let connections = upstream.connections.load(Ordering::SeqCst);
    assert!(
        response.starts_with("HTTP/1.1 400") && log.is_empty() && connections == 0,
        "{head:?}: response {response:?}, log {log:?}, {connections} upstream connections"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn empty_host_plain_http_is_rejected() {
    assert_empty_host_rejected(|port| format!("GET http://:{port}/x HTTP/1.1\r\nHost: x\r\n"))
        .await;
}

#[tokio::test]
async fn empty_host_connect_is_rejected() {
    assert_empty_host_rejected(|port| format!("CONNECT :{port} HTTP/1.1\r\nHost: :{port}\r\n"))
        .await;
}

#[tokio::test]
async fn bracketed_non_address_hosts_are_rejected() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    for target in ["[::1%25eth0]:9", "[not-an-ip]:9"] {
        for response in proxy_plain_and_connect(proxy.local_addr, target).await {
            assert!(
                response.starts_with("HTTP/1.1 400"),
                "{target}: response was {response:?}"
            );
        }
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.is_empty(),
        "rejected targets reached the rules: {log:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn plain_http_invalid_port_is_rejected() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    for target in ["127.0.0.1:65536", "127.0.0.1:abc", "127.0.0.1:+80"] {
        let head = format!("GET http://{target}/path HTTP/1.1\r\nHost: x\r\n");
        let response = proxy_raw_request(proxy.local_addr, head.as_bytes()).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{target}: response was {response:?}"
        );
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.is_empty(),
        "rejected requests reached the rules: {log:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn connect_invalid_port_is_rejected() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    for target in ["127.0.0.1:65536", "127.0.0.1:abc", "127.0.0.1:+80"] {
        let head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
        let response = proxy_raw_request(proxy.local_addr, head.as_bytes()).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{target}: response was {response:?}"
        );
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.is_empty(),
        "rejected requests reached the rules: {log:?}"
    );

    proxy.task.abort();
}

/// An empty port is the scheme default, not an invalid port: the request
/// reaches the rules (and is blocked there, so nothing connects to port 80
/// or 443 of this machine).
#[tokio::test]
async fn empty_port_reaches_rules_as_default_port() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let config_json = proxy_config_json("denylist", &[rule("127.0.0.1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    let [plain, connect] = proxy_plain_and_connect(proxy.local_addr, "127.0.0.1:").await;
    for response in [plain, connect] {
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "response was {response:?}"
        );
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert_eq!(
        log.matches("BLOCKED\t127.0.0.1\t/").count(),
        2,
        "log was {log:?}"
    );
    assert!(log.contains("BLOCKED\t127.0.0.1\t/x"), "log was {log:?}");

    proxy.task.abort();
}

#[tokio::test]
async fn connect_with_userinfo_is_rejected_before_rules() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[rule("127.0.0.1", &[], "block")], &log_path);
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    for userinfo in ["u:secret", "secret"] {
        let target = format!("{userinfo}@127.0.0.1:{port}");
        let head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
        let response = proxy_raw_request(proxy.local_addr, head.as_bytes()).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{target}: response was {response:?}"
        );
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(!log.contains("secret"), "userinfo logged: {log:?}");

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_obs_text_token_lines_are_rejected() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    let request_line = format!("GET http://127.0.0.1:{port}/x HTTP/1.1\r\nHost: x\r\n");
    let token_lines: [&[u8]; 2] = [
        b"Connection: HTTP2-Settings, \xff\r\nHTTP2-Settings: AAA\r\n",
        b"Connection: Upgrade\r\nUpgrade: \xffh2c\r\n",
    ];
    for lines in token_lines {
        let head = [request_line.as_bytes(), lines].concat();
        let response = proxy_raw_request(proxy.local_addr, &head).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{}: response was {response:?}",
            String::from_utf8_lossy(lines)
        );
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_non_websocket_upgrade_is_forwarded_without_upgrade_headers() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr)
        .await
        .expect("connect to proxy");
    let port = upstream.addr.port();
    let request = format!(
        "GET http://127.0.0.1:{port}/h2c HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Connection: Upgrade, HTTP2-Settings\r\nConnection: close\r\n\
         Upgrade: h2c\r\nHTTP2-Settings: AAMAAABkAAQAoAAAAAIAAAAA\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_http_response(&mut stream),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default();

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "response was {response:?}"
    );
    let lower = response.to_ascii_lowercase();
    assert!(
        !lower.contains("upgrade"),
        "upgrade headers reached upstream: {response:?}"
    );
    assert!(
        !lower.contains("http2-settings"),
        "upgrade headers reached upstream: {response:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

/// Upstream that records request heads, answers upgrade requests, and echoes
/// bytes after a `101`.
struct WebSocketUpstream {
    addr: SocketAddr,
    task: JoinHandle<()>,
    /// Request heads received, lowercased.
    heads: Arc<std::sync::Mutex<Vec<String>>>,
    /// Bytes received after the `101`.
    spliced: Arc<AtomicUsize>,
}

/// `upgrade_reply` is the `Upgrade` value the `101` carries.
async fn start_websocket_echo_upstream(upgrade_reply: &'static str) -> WebSocketUpstream {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind websocket upstream");
    let addr = listener.local_addr().expect("websocket upstream addr");
    let heads: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let spliced = Arc::new(AtomicUsize::new(0));
    let (heads_in, spliced_in) = (heads.clone(), spliced.clone());
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let heads = heads_in.clone();
            let spliced = spliced_in.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                if let Ok(mut heads) = heads.lock() {
                    heads.push(head.clone());
                }
                if !head.contains("upgrade: websocket") {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let reply = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: {upgrade_reply}\r\nConnection: Upgrade\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let mut buf = [0_u8; 1024];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    spliced.fetch_add(n, Ordering::SeqCst);
                    if stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    WebSocketUpstream {
        addr,
        task,
        heads,
        spliced,
    }
}

/// Send a plain-HTTP upgrade request through the proxy and read the response
/// head. `extra` holds further header lines, each ending in CRLF.
async fn proxy_upgrade_request(
    proxy_addr: SocketAddr,
    upstream_addr: SocketAddr,
    upgrade: &str,
    extra: &str,
) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    let port = upstream_addr.port();
    let request = format!(
        "GET http://127.0.0.1:{port}/ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Connection: Upgrade\r\nUpgrade: {upgrade}\r\n{extra}\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_exact(&mut byte),
        )
        .await;
        if !matches!(read, Ok(Ok(_))) {
            break;
        }
        head.push(byte[0]);
    }
    (stream, String::from_utf8_lossy(&head).to_string())
}

#[tokio::test]
async fn plain_http_websocket_upgrade_is_relayed() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_websocket_echo_upstream("websocket").await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let (mut stream, head) =
        proxy_upgrade_request(proxy.local_addr, upstream.addr, "websocket", "").await;
    assert!(head.starts_with("HTTP/1.1 101"), "response was {head:?}");

    stream.write_all(b"ping").await.expect("write frame");
    let mut echoed = [0_u8; 4];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut echoed),
    )
    .await
    .expect("echo timeout")
    .expect("read echo");
    assert_eq!(&echoed, b"ping");

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_websocket_upgrade_offers_only_websocket_upstream() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_websocket_echo_upstream("websocket").await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    // HTTP2-Settings is not listed in Connection, so only the explicit
    // removal keeps it from the upstream.
    let (stream, head) = proxy_upgrade_request(
        proxy.local_addr,
        upstream.addr,
        "websocket, h2c",
        "HTTP2-Settings: AAA\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 101"), "response was {head:?}");
    let heads = upstream.heads.lock().map(|h| h.clone()).unwrap_or_default();
    assert_eq!(heads.len(), 1, "heads were {heads:?}");
    let upgrade_lines: Vec<&str> = heads[0]
        .lines()
        .filter(|l| l.starts_with("upgrade:"))
        .collect();
    assert_eq!(upgrade_lines, ["upgrade: websocket"]);
    assert!(
        !heads[0].contains("http2-settings"),
        "HTTP2-Settings reached the upstream: {heads:?}"
    );

    drop(stream);
    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_upgrade_to_other_protocol_is_not_relayed() {
    let log_dir = TempDir::new().expect("temp log dir");
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_websocket_echo_upstream("h2c").await;
    let config_json = proxy_config_json("denylist", &[], &log_path);
    let proxy = start_proxy(&config_json).await;

    let (mut stream, head) =
        proxy_upgrade_request(proxy.local_addr, upstream.addr, "websocket", "").await;
    assert!(head.starts_with("HTTP/1.1 502"), "response was {head:?}");

    // A follow-up request on the same socket must never reach the upstream.
    let port = upstream.addr.port();
    let follow_up =
        format!("GET http://127.0.0.1:{port}/second HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    let _ = stream.write_all(follow_up.as_bytes()).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(upstream.spliced.load(Ordering::SeqCst), 0);
    let heads = upstream.heads.lock().map(|h| h.len()).unwrap_or_default();
    assert_eq!(heads, 1);

    proxy.task.abort();
    upstream.task.abort();
}

// ---- MITM integration test infrastructure ----

struct TestCa {
    cert_pem: String,
    key_pem: String,
    key: KeyPair,
    params: CertificateParams,
}

static TEST_CA: OnceLock<TestCa> = OnceLock::new();

fn test_ca() -> &'static TestCa {
    TEST_CA.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let key = KeyPair::generate().unwrap();
        let params = cella_network::ca::ca_certificate_params();
        let cert = params.self_signed(&key).unwrap();

        TestCa {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            key,
            params,
        }
    })
}

fn generate_server_cert(
    ca: &TestCa,
    domain: &str,
) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let issuer = Issuer::from_params(&ca.params, &ca.key);
    let server_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::NoCa;
    params
        .distinguished_name
        .push(DnType::CommonName, DnValue::Utf8String(domain.to_string()));
    params
        .subject_alt_names
        .push(SanType::DnsName(domain.to_string().try_into().unwrap()));
    let cert = params.signed_by(&server_key, &issuer).unwrap();
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
    (vec![cert_der], key_der)
}

struct TlsUpstreamServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

async fn start_tls_upstream(
    ca: &TestCa,
    domain: &str,
    handler: fn() -> (u16, Vec<u8>),
) -> TlsUpstreamServer {
    start_tls_upstream_with_headers(ca, domain, handler, "").await
}

/// Like [`start_tls_upstream`], with `extra_headers` (complete `name: value\r\n`
/// lines) added to every response.
async fn start_tls_upstream_with_headers(
    ca: &TestCa,
    domain: &str,
    handler: fn() -> (u16, Vec<u8>),
    extra_headers: &'static str,
) -> TlsUpstreamServer {
    serve_tls_upstream(ca, domain, handler, extra_headers, false).await
}

/// Like [`start_tls_upstream`], but closes each connection after one
/// response without announcing it in a `Connection` header.
async fn start_closing_tls_upstream(
    ca: &TestCa,
    domain: &str,
    handler: fn() -> (u16, Vec<u8>),
) -> TlsUpstreamServer {
    serve_tls_upstream(ca, domain, handler, "", true).await
}

async fn serve_tls_upstream(
    ca: &TestCa,
    domain: &str,
    handler: fn() -> (u16, Vec<u8>),
    extra_headers: &'static str,
    close_after_response: bool,
) -> TlsUpstreamServer {
    let (certs, key) = generate_server_cert(ca, domain);
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                loop {
                    let mut headers = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        if tls.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        headers.push(byte[0]);
                        if headers.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let (status, body) = handler();
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\n{extra_headers}Content-Length: {}\r\n\r\n",
                        body.len()
                    );
                    if tls.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                    if tls.write_all(&body).await.is_err() {
                        return;
                    }
                    if close_after_response {
                        let _ = tls.shutdown().await;
                        return;
                    }
                }
            });
        }
    });

    TlsUpstreamServer { addr, task }
}

fn mitm_proxy_config_json(mode: &str, rules: &[Value], log_path: &Path, ca: &TestCa) -> String {
    serde_json::json!({
        "listen_port": 0,
        "mode": mode,
        "rules": rules,
        "upstream_proxy": null,
        "ca_cert_pem": ca.cert_pem,
        "ca_key_pem": ca.key_pem,
        "log_path": log_path.to_string_lossy().into_owned(),
    })
    .to_string()
}

async fn proxy_connect_tls_and_request(
    proxy_addr: SocketAddr,
    host: &str,
    port: u16,
    path: &str,
    ca: &TestCa,
) -> Result<String, String> {
    let mut stream = TcpStream::connect(proxy_addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    let connect_req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream
        .write_all(connect_req.as_bytes())
        .await
        .map_err(|e| format!("write CONNECT: {e}"))?;

    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let connect_resp = String::from_utf8_lossy(&headers).to_string();
    if !connect_resp.starts_with("HTTP/1.1 200") {
        return Ok(connect_resp);
    }

    let cert_der = CertificateDer::from(
        ca.params
            .clone()
            .self_signed(&ca.key)
            .unwrap()
            .der()
            .to_vec(),
    );
    let mut root_store = rustls::RootCertStore::empty();
    let _ = root_store.add(cert_der);

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let mut tls = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| format!("client TLS: {e}"))?;

    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n");
    tls.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write request: {e}"))?;

    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while tls.read_exact(&mut byte).await.is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let headers_str = String::from_utf8_lossy(&headers).to_string();
    let cl = headers_str
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; cl];
    if cl > 0 {
        tls.read_exact(&mut body)
            .await
            .map_err(|e| format!("read body: {e}"))?;
    }

    let mut full_response = headers;
    full_response.extend_from_slice(&body);
    Ok(String::from_utf8_lossy(&full_response).to_string())
}

async fn start_h2_tls_upstream(ca: &TestCa, domain: &str) -> TlsUpstreamServer {
    use http_body_util::Full as FullBody;
    use hyper::body::Bytes;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let (certs, key) = generate_server_cert(ca, domain);
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let svc = hyper::service::service_fn(|_req| async {
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(FullBody::new(
                        Bytes::from("h2 upstream ok"),
                    )))
                });
                let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                let _ = builder.serve_connection(TokioIo::new(tls), svc).await;
            });
        }
    });

    TlsUpstreamServer { addr, task }
}

async fn start_tls_upgrade_upstream(ca: &TestCa, domain: &str) -> TlsUpstreamServer {
    let (certs, key) = generate_server_cert(ca, domain);
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut headers = Vec::new();
                let mut byte = [0u8; 1];
                while tls.read_exact(&mut byte).await.is_ok() {
                    headers.push(byte[0]);
                    if headers.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let headers_str = String::from_utf8_lossy(&headers);
                let has_upgrade = headers_str
                    .lines()
                    .any(|l| l.to_ascii_lowercase().starts_with("upgrade:"));
                if has_upgrade {
                    let resp = "HTTP/1.1 101 Switching Protocols\r\n\
                        Upgrade: websocket\r\n\
                        Connection: Upgrade\r\n\r\n";
                    let _ = tls.write_all(resp.as_bytes()).await;
                    let mut buf = [0u8; 1024];
                    loop {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let _ = tls.write_all(&buf[..n]).await;
                            }
                        }
                    }
                } else {
                    let body = b"no upgrade";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = tls.write_all(resp.as_bytes()).await;
                    let _ = tls.write_all(body).await;
                }
            });
        }
    });

    TlsUpstreamServer { addr, task }
}

fn simple_200_handler() -> (u16, Vec<u8>) {
    (200, b"upstream ok".to_vec())
}

fn large_body_handler() -> (u16, Vec<u8>) {
    (200, vec![0x42; 256 * 1024])
}

#[tokio::test]
async fn mitm_allows_and_relays_response() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_tls_upstream(ca, "localhost", simple_200_handler).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/blocked/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(
        proxy.local_addr,
        "localhost",
        upstream.addr.port(),
        "/allowed",
        ca,
    )
    .await
    .unwrap();

    assert!(resp.contains("200"), "expected 200, got {resp:?}");
    assert!(resp.contains("upstream ok"), "expected body, got {resp:?}");

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_blocks_matching_path() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_tls_upstream(ca, "localhost", simple_200_handler).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/blocked/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(
        proxy.local_addr,
        "localhost",
        upstream.addr.port(),
        "/blocked/secret",
        ca,
    )
    .await
    .unwrap();

    assert!(resp.contains("403"), "expected 403, got {resp:?}");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("BLOCKED\tlocalhost\t/blocked/secret"));

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_relays_large_response() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_tls_upstream(ca, "localhost", large_body_handler).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(
        proxy.local_addr,
        "localhost",
        upstream.addr.port(),
        "/large",
        ca,
    )
    .await
    .unwrap();

    assert!(resp.contains("200"), "expected 200, got {resp:?}");
    let body_start = resp.find("\r\n\r\n").unwrap() + 4;
    let body = &resp.as_bytes()[body_start..];
    assert_eq!(
        body.len(),
        256 * 1024,
        "expected 256KB body, got {} bytes",
        body.len()
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_handles_many_keepalive_cycles() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_tls_upstream(ca, "localhost", simple_200_handler).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr).await.unwrap();
    let connect_req = format!(
        "CONNECT localhost:{} HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
        upstream.addr.port(),
        upstream.addr.port(),
    );
    stream.write_all(connect_req.as_bytes()).await.unwrap();

    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let cert_der = CertificateDer::from(
        ca.params
            .clone()
            .self_signed(&ca.key)
            .unwrap()
            .der()
            .to_vec(),
    );
    let mut root_store = rustls::RootCertStore::empty();
    let _ = root_store.add(cert_der);
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls = connector.connect(server_name, stream).await.unwrap();

    for i in 0..20 {
        let req = format!("GET /req/{i} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        tls.write_all(req.as_bytes()).await.unwrap();

        let mut resp_headers = Vec::new();
        let mut b = [0u8; 1];
        while tls.read_exact(&mut b).await.is_ok() {
            resp_headers.push(b[0]);
            if resp_headers.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let resp_str = String::from_utf8_lossy(&resp_headers);
        assert!(resp_str.contains("200"), "request {i} failed: {resp_str}");

        let cl = resp_str
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if cl > 0 {
            let mut body = vec![0u8; cl];
            tls.read_exact(&mut body).await.unwrap();
        }
    }

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_relays_through_h2_upstream() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_h2_tls_upstream(ca, "localhost").await;
    // Path rule triggers MITM interception (domain-only rules skip MITM).
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(
        proxy.local_addr,
        "localhost",
        upstream.addr.port(),
        "/api/test",
        ca,
    )
    .await
    .unwrap();

    assert!(resp.contains("200"), "expected 200, got {resp:?}");
    assert!(
        resp.contains("h2 upstream ok"),
        "expected h2 body, got {resp:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_relays_websocket_upgrade() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_tls_upgrade_upstream(ca, "localhost").await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/blocked/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr).await.unwrap();
    let connect_req = format!(
        "CONNECT localhost:{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        upstream.addr.port()
    );
    stream.write_all(connect_req.as_bytes()).await.unwrap();
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let connect_resp = String::from_utf8_lossy(&headers);
    assert!(
        connect_resp.starts_with("HTTP/1.1 200"),
        "CONNECT failed: {connect_resp:?}"
    );

    let cert_der = CertificateDer::from(
        ca.params
            .clone()
            .self_signed(&ca.key)
            .unwrap()
            .der()
            .to_vec(),
    );
    let mut root_store = rustls::RootCertStore::empty();
    let _ = root_store.add(cert_der);
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls = connector.connect(server_name, stream).await.unwrap();

    let req = "GET /allowed HTTP/1.1\r\n\
        Host: localhost\r\n\
        Upgrade: websocket\r\n\
        Connection: Upgrade\r\n\r\n";
    tls.write_all(req.as_bytes()).await.unwrap();

    let mut resp_headers = Vec::new();
    while tls.read_exact(&mut byte).await.is_ok() {
        resp_headers.push(byte[0]);
        if resp_headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let resp_str = String::from_utf8_lossy(&resp_headers);
    assert!(resp_str.contains("101"), "expected 101, got {resp_str:?}");

    let test_data = b"hello websocket";
    tls.write_all(test_data).await.unwrap();
    let mut echoed = vec![0u8; test_data.len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tls.read_exact(&mut echoed),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&echoed, test_data, "echoed data mismatch");

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_blocks_upgrade_on_blocked_path() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");

    let upstream = start_tls_upgrade_upstream(ca, "localhost").await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/blocked/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let mut stream = TcpStream::connect(proxy.local_addr).await.unwrap();
    let connect_req = format!(
        "CONNECT localhost:{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        upstream.addr.port()
    );
    stream.write_all(connect_req.as_bytes()).await.unwrap();
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let connect_resp = String::from_utf8_lossy(&headers);
    assert!(
        connect_resp.starts_with("HTTP/1.1 200"),
        "CONNECT failed: {connect_resp:?}"
    );

    let cert_der = CertificateDer::from(
        ca.params
            .clone()
            .self_signed(&ca.key)
            .unwrap()
            .der()
            .to_vec(),
    );
    let mut root_store = rustls::RootCertStore::empty();
    let _ = root_store.add(cert_der);
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls = connector.connect(server_name, stream).await.unwrap();

    let req = "GET /blocked/ws HTTP/1.1\r\n\
        Host: localhost\r\n\
        Upgrade: websocket\r\n\
        Connection: Upgrade\r\n\r\n";
    tls.write_all(req.as_bytes()).await.unwrap();

    let mut resp_headers = Vec::new();
    while tls.read_exact(&mut byte).await.is_ok() {
        resp_headers.push(byte[0]);
        if resp_headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let resp_str = String::from_utf8_lossy(&resp_headers);
    assert!(
        resp_str.contains("403"),
        "expected 403 for blocked path, got {resp_str:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

const WEBSOCKET_SWITCH: &str =
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
const H2C_SWITCH: &str =
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: h2c\r\nConnection: Upgrade\r\n\r\n";

/// TLS upstream on keep-alive connections that answers a request carrying
/// `Upgrade` with `upgrade_reply`, echoing bytes after it when it is a `101`,
/// and any other request with `200` and body `no upgrade`.
async fn start_recording_tls_upstream(
    ca: &TestCa,
    domain: &str,
    upgrade_reply: &'static str,
) -> WebSocketUpstream {
    let (certs, key) = generate_server_cert(ca, domain);
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let spliced = Arc::new(AtomicUsize::new(0));
    let (heads_in, spliced_in) = (heads.clone(), spliced.clone());
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, heads, spliced) =
                (acceptor.clone(), heads_in.clone(), spliced_in.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                loop {
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if tls.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    if let Ok(mut heads) = heads.lock() {
                        heads.push(head.clone());
                    }
                    let reply = if head.lines().any(|l| l.starts_with("upgrade:")) {
                        upgrade_reply
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nno upgrade"
                    };
                    if tls.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                    if reply.starts_with("HTTP/1.1 101") {
                        break;
                    }
                }
                let mut buf = [0_u8; 1024];
                while let Ok(n) = tls.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    spliced.fetch_add(n, Ordering::SeqCst);
                    if tls.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    WebSocketUpstream {
        addr,
        task,
        heads,
        spliced,
    }
}

/// Start a [`start_recording_tls_upstream`] on `localhost` and a MITM proxy
/// blocking `/blocked/**` there, and open a client TLS connection to the
/// upstream through the proxy.
async fn start_mitm_upgrade_proxy(
    log_path: &Path,
    upgrade_reply: &'static str,
) -> (
    WebSocketUpstream,
    ForwardProxyHandle,
    tokio_rustls::client::TlsStream<TcpStream>,
) {
    let ca = test_ca();
    let upstream = start_recording_tls_upstream(ca, "localhost", upgrade_reply).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/blocked/**"], "block")],
        log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;
    let tls = connect_mitm_tls(proxy.local_addr, "localhost", upstream.addr.port(), ca).await;
    (upstream, proxy, tls)
}

/// Send `request` on an open connection and read one response.
async fn exchange<S>(stream: &mut S, request: &[u8]) -> String
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    stream.write_all(request).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_http_response(stream),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

#[tokio::test]
async fn mitm_non_websocket_upgrade_is_forwarded_without_upgrade_headers() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let (upstream, proxy, mut tls) = start_mitm_upgrade_proxy(&log_path, H2C_SWITCH).await;

    // HTTP2-Settings is not listed in Connection, so only the explicit
    // removal keeps it from the upstream.
    let resp = exchange(
        &mut tls,
        b"GET /h2c HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: h2c\r\n\
          HTTP2-Settings: AAA\r\n\r\n",
    )
    .await;
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.ends_with("no upgrade"),
        "response was {resp:?}"
    );
    let heads = upstream.heads.lock().map(|h| h.clone()).unwrap_or_default();
    assert_eq!(heads.len(), 1, "heads were {heads:?}");
    for name in ["upgrade", "http2-settings"] {
        assert!(
            !heads[0].contains(name),
            "{name} reached upstream: {heads:?}"
        );
    }

    // The connection was not spliced, so a later request is still evaluated.
    let blocked = exchange(
        &mut tls,
        b"GET /blocked/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .await;
    assert!(
        blocked.starts_with("HTTP/1.1 403"),
        "response was {blocked:?}"
    );
    assert_eq!(upstream.spliced.load(Ordering::SeqCst), 0);
    let heads = upstream.heads.lock().map(|h| h.len()).unwrap_or_default();
    assert_eq!(heads, 1);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_websocket_upgrade_offers_only_websocket_upstream() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let (upstream, proxy, mut tls) = start_mitm_upgrade_proxy(&log_path, WEBSOCKET_SWITCH).await;

    // HTTP2-Settings is not listed in Connection, so only the explicit
    // removal keeps it from the upstream.
    let resp = exchange(
        &mut tls,
        b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\n\
          Upgrade: websocket, h2c\r\nHTTP2-Settings: AAA\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 101"), "response was {resp:?}");
    let heads = upstream.heads.lock().map(|h| h.clone()).unwrap_or_default();
    assert_eq!(heads.len(), 1, "heads were {heads:?}");
    let upgrade_lines: Vec<&str> = heads[0]
        .lines()
        .filter(|l| l.starts_with("upgrade:"))
        .collect();
    assert_eq!(upgrade_lines, ["upgrade: websocket"]);
    assert!(
        !heads[0].contains("http2-settings"),
        "HTTP2-Settings reached the upstream: {heads:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_upgrade_to_other_protocol_is_not_relayed() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let (upstream, proxy, mut tls) = start_mitm_upgrade_proxy(&log_path, H2C_SWITCH).await;

    let resp = exchange(
        &mut tls,
        b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 502"), "response was {resp:?}");

    // Bytes written after the refusal must never reach the upstream.
    let _ = tls
        .write_all(b"GET /second HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(upstream.spliced.load(Ordering::SeqCst), 0);
    let heads = upstream.heads.lock().map(|h| h.len()).unwrap_or_default();
    assert_eq!(heads, 1);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_upgrade_refusal_drops_hop_by_hop_headers() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let refusal = "HTTP/1.1 426 Upgrade Required\r\nConnection: x-hop\r\nX-Hop: 1\r\n\
                   Keep-Alive: timeout=5\r\nX-Kept: 1\r\nContent-Length: 0\r\n\r\n";
    let (upstream, proxy, mut tls) = start_mitm_upgrade_proxy(&log_path, refusal).await;

    let resp = exchange(
        &mut tls,
        b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 426"), "response was {resp:?}");
    for name in ["x-hop", "keep-alive"] {
        assert!(
            header_values(&resp, name).is_empty(),
            "relayed {name}: {resp:?}"
        );
    }
    assert_eq!(header_values(&resp, "x-kept"), ["1"]);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_obs_text_token_lines_are_rejected() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let (upstream, proxy, mut tls) = start_mitm_upgrade_proxy(&log_path, H2C_SWITCH).await;

    let token_lines: [&[u8]; 2] = [
        b"Connection: HTTP2-Settings, \xff\r\nHTTP2-Settings: AAA\r\n",
        b"Connection: Upgrade\r\nUpgrade: \xffh2c\r\n",
    ];
    for lines in token_lines {
        let request = [
            b"GET /x HTTP/1.1\r\nHost: localhost\r\n".as_slice(),
            lines,
            b"\r\n",
        ]
        .concat();
        let resp = exchange(&mut tls, &request).await;
        assert!(
            resp.starts_with("HTTP/1.1 400"),
            "{}: response was {resp:?}",
            lines.escape_ascii()
        );
    }
    let heads = upstream.heads.lock().map(|h| h.clone()).unwrap_or_default();
    assert!(heads.is_empty(), "heads were {heads:?}");

    proxy.task.abort();
    upstream.task.abort();
}

// ---- respond rules and response headers ----

fn respond_rule(domain: &str, paths: &[&str], responses: &[Value]) -> Value {
    let mut rule = rule(domain, paths, "respond");
    rule["responses"] = Value::from(responses.to_vec());
    rule
}

fn allow_rule_with_headers(domain: &str, headers: &[(&str, &str)]) -> Value {
    let mut rule = rule(domain, &[], "allow");
    rule["response_headers"] = headers
        .iter()
        .map(|(name, value)| ((*name).to_string(), Value::from(*value)))
        .collect::<serde_json::Map<_, _>>()
        .into();
    rule
}

/// Every value of header `name` in a raw HTTP/1.1 response.
fn header_values(response: &str, name: &str) -> Vec<String> {
    response
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
        .collect()
}

/// A loopback port nothing listens on.
async fn unused_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    listener.local_addr().unwrap().port()
}

/// CONNECT through the proxy and complete TLS with its MITM certificate.
async fn connect_mitm_tls(
    proxy_addr: SocketAddr,
    host: &str,
    port: u16,
    ca: &TestCa,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    connect_mitm_tls_alpn(proxy_addr, host, port, ca, b"http/1.1").await
}

/// Like [`connect_mitm_tls`], offering only `alpn`.
async fn connect_mitm_tls_alpn(
    proxy_addr: SocketAddr,
    host: &str,
    port: u16,
    ca: &TestCa,
    alpn: &[u8],
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
    let connect_req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream.write_all(connect_req.as_bytes()).await.unwrap();
    let connect_resp = read_http_response(&mut stream).await.unwrap_or_default();
    assert!(
        connect_resp.starts_with("HTTP/1.1 200"),
        "CONNECT failed: {connect_resp:?}"
    );

    let cert_der = CertificateDer::from(
        ca.params
            .clone()
            .self_signed(&ca.key)
            .unwrap()
            .der()
            .to_vec(),
    );
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(cert_der).unwrap();
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![alpn.to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    connector.connect(server_name, stream).await.unwrap()
}

/// POST `body_len` bytes to `path` on an open keep-alive connection and read
/// the response.
async fn post_keepalive<S>(stream: &mut S, host: &str, path: &str, body_len: usize) -> String
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&vec![b'x'; body_len]).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_http_response(stream),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

#[tokio::test]
async fn plain_http_respond_serves_configured_response() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let response = serde_json::json!({
        "status": 529,
        "headers": {"content-type": "application/json", "retry-after": "0"},
        "body": r#"{"type":"error"}"#,
    });
    let config_json = proxy_config_json(
        "denylist",
        &[respond_rule("127.0.0.1", &["/v1/messages"], &[response])],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_http_request(proxy.local_addr, upstream.addr, "/v1/messages").await;
    assert!(resp.starts_with("HTTP/1.1 529"), "response was {resp:?}");
    assert_eq!(header_values(&resp, "content-type"), ["application/json"]);
    assert_eq!(header_values(&resp, "retry-after"), ["0"]);
    let request_ids = header_values(&resp, "request-id");
    assert_eq!(request_ids.len(), 1, "response was {resp:?}");
    assert!(request_ids[0].starts_with("req_cella_"), "{request_ids:?}");
    assert!(
        resp.ends_with(r#"{"type":"error"}"#),
        "response was {resp:?}"
    );
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_respond_sends_configured_request_id_once() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let response = serde_json::json!({
        "status": 200,
        "headers": {"Request-Id": "req_fixed_123"},
    });
    let config_json = proxy_config_json(
        "denylist",
        &[respond_rule("127.0.0.1", &[], &[response])],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_http_request(proxy.local_addr, upstream.addr, "/v1/messages").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "response was {resp:?}");
    assert_eq!(header_values(&resp, "request-id"), ["req_fixed_123"]);

    proxy.task.abort();
    upstream.task.abort();
}

/// Start the forward proxy with a `respond` drain timeout of `timeout`.
async fn start_proxy_with_drain_timeout(
    config_json: &str,
    timeout: std::time::Duration,
) -> ForwardProxyHandle {
    let mut config = AgentProxyConfig::from_json(config_json).expect("parse proxy config");
    config.respond_drain_timeout = timeout;
    forward_proxy::start_forward_proxy(Arc::new(config))
        .await
        .expect("start forward proxy")
}

/// A config answering every request to 127.0.0.1 with `529 busy`.
fn busy_respond_config(log_path: &Path) -> String {
    let response = serde_json::json!({"status": 529, "body": "busy"});
    proxy_config_json(
        "denylist",
        &[respond_rule("127.0.0.1", &[], &[response])],
        log_path,
    )
}

#[tokio::test]
async fn plain_http_respond_answers_expect_continue_without_the_body() {
    let log_dir = TempDir::new().unwrap();
    let proxy = start_proxy(&busy_respond_config(&log_dir.path().join("proxy.log"))).await;
    let port = unused_port().await;

    let mut stream = TcpStream::connect(proxy.local_addr).await.unwrap();
    let head = format!(
        "POST http://127.0.0.1:{port}/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nExpect: 100-Continue\r\nContent-Length: 100\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_to_end(&mut raw),
    )
    .await;
    let resp = String::from_utf8_lossy(&raw).into_owned();
    assert!(read.is_ok(), "connection still open; read {resp:?}");
    assert!(resp.starts_with("HTTP/1.1 529"), "response was {resp:?}");
    assert!(!resp.contains("100 Continue"), "response was {resp:?}");
    assert_eq!(header_values(&resp, "connection"), ["close"]);
    assert!(resp.ends_with("busy"), "response was {resp:?}");

    proxy.task.abort();
}

#[tokio::test]
async fn plain_http_respond_answers_a_stalled_body_after_the_drain_timeout() {
    let log_dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_drain_timeout(
        &busy_respond_config(&log_dir.path().join("proxy.log")),
        std::time::Duration::from_millis(300),
    )
    .await;
    let port = unused_port().await;

    let mut stream = TcpStream::connect(proxy.local_addr).await.unwrap();
    let head = format!(
        "POST http://127.0.0.1:{port}/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1000\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&[b'x'; 10]).await.unwrap();

    let mut first = [0_u8; 1];
    let early = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        stream.read(&mut first),
    )
    .await;
    assert!(early.is_err(), "answered before the drain timeout");

    let mut raw = Vec::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(750),
        stream.read_to_end(&mut raw),
    )
    .await;
    let resp = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        read.is_ok(),
        "not answered and closed by 1 s; read {resp:?}"
    );
    assert!(resp.starts_with("HTTP/1.1 529"), "response was {resp:?}");
    assert_eq!(header_values(&resp, "connection"), ["close"]);
    assert!(resp.ends_with("busy"), "response was {resp:?}");

    proxy.task.abort();
}

/// A client that sends part of a body and goes away receives nothing, so it
/// takes no step of the sequence: the next request still gets the first.
#[tokio::test]
async fn plain_http_respond_request_dropped_mid_body_takes_no_step() {
    let log_dir = TempDir::new().unwrap();
    let responses = [
        serde_json::json!({"status": 529, "body": "busy"}),
        serde_json::json!({"status": 200, "body": "ok"}),
    ];
    let config_json = proxy_config_json(
        "denylist",
        &[respond_rule("127.0.0.1", &[], &responses)],
        &log_dir.path().join("proxy.log"),
    );
    // Short enough that a disconnect the drain missed would still produce a
    // response, and take a step, before the next request.
    let proxy =
        start_proxy_with_drain_timeout(&config_json, std::time::Duration::from_millis(300)).await;
    let port = unused_port().await;

    let mut dropped = TcpStream::connect(proxy.local_addr).await.unwrap();
    let head = format!(
        "POST http://127.0.0.1:{port}/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1000\r\n\r\n"
    );
    dropped.write_all(head.as_bytes()).await.unwrap();
    dropped.write_all(&[b'x'; 10]).await.unwrap();
    // Let the proxy match the request and start draining, then go away and
    // let the drain end, by the disconnect or else by its timeout.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(dropped);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    let target = SocketAddr::from(([127, 0, 0, 1], port));
    let second = proxy_http_request(proxy.local_addr, target, "/v1/messages").await;
    assert!(
        second.starts_with("HTTP/1.1 529"),
        "response was {second:?}"
    );
    let third = proxy_http_request(proxy.local_addr, target, "/v1/messages").await;
    assert!(third.starts_with("HTTP/1.1 200"), "response was {third:?}");

    proxy.task.abort();
}

#[tokio::test]
async fn plain_http_respond_to_head_advertises_the_body_length() {
    let log_dir = TempDir::new().unwrap();
    let proxy = start_proxy(&busy_respond_config(&log_dir.path().join("proxy.log"))).await;
    let port = unused_port().await;

    let mut stream = TcpStream::connect(proxy.local_addr).await.unwrap();
    let head = format!(
        "HEAD http://127.0.0.1:{port}/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_to_end(&mut raw),
    )
    .await;
    let resp = String::from_utf8_lossy(&raw).into_owned();
    assert!(read.is_ok(), "connection still open; read {resp:?}");
    assert!(resp.starts_with("HTTP/1.1 529"), "response was {resp:?}");
    assert_eq!(header_values(&resp, "content-length"), ["4"]);
    assert!(
        resp.ends_with("\r\n\r\n"),
        "HEAD response carried a body: {resp:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn plain_http_allow_injects_response_headers() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let config_json = proxy_config_json(
        "denylist",
        &[allow_rule_with_headers("127.0.0.1", &[("x-test-run", "1")])],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_http_request(proxy.local_addr, upstream.addr, "/anything").await;
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.contains("upstream ok"),
        "response was {resp:?}"
    );
    assert_eq!(header_values(&resp, "x-test-run"), ["1"]);
    assert!(header_values(&resp, "request-id").is_empty());

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_respond_needs_no_upstream_listener() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let port = unused_port().await;
    let response = serde_json::json!({"status": 200, "body": "mocked"});
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[respond_rule("localhost", &[], &[response])],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(proxy.local_addr, "localhost", port, "/v1/x", ca)
        .await
        .unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.ends_with("mocked"),
        "response was {resp:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn mitm_respond_to_head_over_h2_advertises_the_body_length() {
    use http_body_util::{BodyExt, Empty};
    use hyper::body::Bytes;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let port = unused_port().await;
    let response = serde_json::json!({
        "status": 200,
        "headers": {"content-type": "text/plain"},
        "body": "mocked",
    });
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[respond_rule("localhost", &[], &[response])],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let tls = connect_mitm_tls_alpn(proxy.local_addr, "localhost", port, ca, b"h2").await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    let head = hyper::Request::head("https://localhost/v1/x")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(head).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/plain");
    assert_eq!(resp.headers()["content-length"], "6");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty(), "HEAD response carried a body: {body:?}");

    proxy.task.abort();
}

#[tokio::test]
async fn mitm_respond_sequence_over_keepalive_post() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let port = unused_port().await;
    let responses = [
        serde_json::json!({"status": 529, "body": "overloaded"}),
        serde_json::json!({"status": 529, "body": "overloaded"}),
        serde_json::json!({"status": 200, "body": "done"}),
    ];
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[respond_rule("localhost", &["/v1/messages"], &responses)],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let mut tls = connect_mitm_tls(proxy.local_addr, "localhost", port, ca).await;
    let mut statuses = Vec::new();
    let mut request_ids = Vec::new();
    for _ in 0..3 {
        let resp = post_keepalive(&mut tls, "localhost", "/v1/messages", 256 * 1024).await;
        statuses.push(resp.lines().next().unwrap_or_default().to_string());
        request_ids.push(header_values(&resp, "request-id"));
    }
    assert!(statuses[0].starts_with("HTTP/1.1 529"), "{statuses:?}");
    assert!(statuses[1].starts_with("HTTP/1.1 529"), "{statuses:?}");
    assert!(statuses[2].starts_with("HTTP/1.1 200"), "{statuses:?}");
    assert_eq!(request_ids[0].len(), 1, "{request_ids:?}");
    assert_eq!(request_ids[1].len(), 1, "{request_ids:?}");
    assert!(
        request_ids[0][0].starts_with("req_cella_"),
        "{request_ids:?}"
    );
    assert_ne!(request_ids[0], request_ids[1]);

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("RESPONDED\tlocalhost\t/v1/messages\t529\t"),
        "log was {log:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn mitm_allow_injects_and_overrides_response_headers() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_tls_upstream_with_headers(
        ca,
        "localhost",
        simple_200_handler,
        "x-test-run: upstream\r\nx-upstream: kept\r\n",
    )
    .await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[allow_rule_with_headers(
            "localhost",
            &[("x-test-run", "1"), ("x-added", "yes")],
        )],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(
        proxy.local_addr,
        "localhost",
        upstream.addr.port(),
        "/anything",
        ca,
    )
    .await
    .unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.ends_with("upstream ok"),
        "response was {resp:?}"
    );
    assert_eq!(header_values(&resp, "x-test-run"), ["1"]);
    assert_eq!(header_values(&resp, "x-added"), ["yes"]);
    assert_eq!(header_values(&resp, "x-upstream"), ["kept"]);

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn plain_http_proxy_502_carries_no_response_headers() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let port = unused_port().await;
    let config_json = proxy_config_json(
        "denylist",
        &[allow_rule_with_headers("127.0.0.1", &[("x-test-run", "1")])],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let unreachable = SocketAddr::from(([127, 0, 0, 1], port));
    let resp = proxy_http_request(proxy.local_addr, unreachable, "/anything").await;
    assert!(resp.starts_with("HTTP/1.1 502"), "response was {resp:?}");
    assert!(
        header_values(&resp, "x-test-run").is_empty(),
        "response was {resp:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn mitm_proxy_502_carries_no_response_headers() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let port = unused_port().await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[allow_rule_with_headers("localhost", &[("x-test-run", "1")])],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_tls_and_request(proxy.local_addr, "localhost", port, "/x", ca)
        .await
        .unwrap();
    assert!(resp.starts_with("HTTP/1.1 502"), "response was {resp:?}");
    assert!(
        header_values(&resp, "x-test-run").is_empty(),
        "response was {resp:?}"
    );

    proxy.task.abort();
}

#[tokio::test]
async fn mitm_strips_upstream_hop_by_hop_response_headers() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_tls_upstream_with_headers(
        ca,
        "localhost",
        simple_200_handler,
        "Connection: close, x-hop\r\nX-Hop: 1\r\nKeep-Alive: timeout=5\r\nX-Kept: 1\r\n",
    )
    .await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let mut tls = connect_mitm_tls(proxy.local_addr, "localhost", upstream.addr.port(), ca).await;
    for i in 0..2 {
        let req = format!("GET /req/{i} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        tls.write_all(req.as_bytes()).await.unwrap();
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_http_response(&mut tls),
        )
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        assert!(
            resp.starts_with("HTTP/1.1 200") && resp.ends_with("upstream ok"),
            "request {i} got {resp:?}"
        );
        for name in ["connection", "x-hop", "keep-alive"] {
            assert!(
                header_values(&resp, name).is_empty(),
                "request {i} relayed {name}: {resp:?}"
            );
        }
        assert_eq!(header_values(&resp, "x-kept"), ["1"]);
    }

    proxy.task.abort();
    upstream.task.abort();
}

/// A request after the upstream closed its connection goes out on a fresh
/// one. A connection closing just as a request is handed to it is a race over
/// real sockets; the `mitm::tests::resends_*` tests cover that resend.
#[tokio::test]
async fn mitm_opens_a_fresh_connection_after_upstream_closes() {
    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_closing_tls_upstream(ca, "localhost", simple_200_handler).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let mut tls = connect_mitm_tls(proxy.local_addr, "localhost", upstream.addr.port(), ca).await;
    for i in 0..2 {
        let req = format!("GET /req/{i} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        tls.write_all(req.as_bytes()).await.unwrap();
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_http_response(&mut tls),
        )
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        assert!(
            resp.starts_with("HTTP/1.1 200") && resp.ends_with("upstream ok"),
            "request {i} got {resp:?}"
        );
        // Let the proxy see the upstream close before the next request.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn connect_respond_rule_without_mitm_blocks_tunnel() {
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let upstream = start_upstream_server().await;
    let response = serde_json::json!({"status": 529});
    // No CA material, so the respond rule cannot be applied by decrypting.
    let config_json = proxy_config_json(
        "denylist",
        &[respond_rule("127.0.0.1", &["/v1/messages"], &[response])],
        &log_path,
    );
    let proxy = start_proxy(&config_json).await;

    let resp = proxy_connect_and_tunnel(proxy.local_addr, upstream.addr, "/v1/messages").await;
    assert!(
        resp.starts_with("HTTP/1.1 403"),
        "respond rule without MITM must block the CONNECT, got {resp:?}"
    );
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("BLOCKED\t127.0.0.1\t/\trespond rule requires TLS interception"),
        "log was {log:?}"
    );

    proxy.task.abort();
    upstream.task.abort();
}

/// HTTP/1.1 TLS upstream that answers `/stream` with its headers and half its
/// body, sending the rest only once `release` is notified; any other path
/// gets `other` straight away. `accepted` counts the connections it accepts.
async fn start_held_stream_tls_upstream(
    ca: &TestCa,
    domain: &str,
    release: Arc<tokio::sync::Notify>,
    accepted: Arc<AtomicUsize>,
) -> TlsUpstreamServer {
    let (certs, key) = generate_server_cert(ca, domain);
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            let release = release.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                while let Some(head) = read_http_response(&mut tls).await {
                    let request_line = head.lines().next().unwrap_or_default().to_string();
                    if request_line.contains("/stream ") {
                        let first = "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello";
                        tls.write_all(first.as_bytes()).await.unwrap();
                        tls.flush().await.unwrap();
                        release.notified().await;
                        tls.write_all(b"world").await.unwrap();
                    } else {
                        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nother";
                        tls.write_all(resp.as_bytes()).await.unwrap();
                    }
                    tls.flush().await.unwrap();
                }
            });
        }
    });

    TlsUpstreamServer { addr, task }
}

#[tokio::test]
async fn mitm_h2_stream_is_not_held_up_by_open_h1_response() {
    use std::time::Duration;

    use http_body_util::{BodyExt, Empty};
    use hyper::body::Bytes;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let release = Arc::new(tokio::sync::Notify::new());
    let accepted = Arc::new(AtomicUsize::new(0));
    let upstream = start_held_stream_tls_upstream(ca, "localhost", release.clone(), accepted).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    let tls = connect_mitm_tls_alpn(proxy.local_addr, "localhost", port, ca, b"h2").await;
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    let get = |path: &str| {
        hyper::Request::get(format!("https://localhost{path}"))
            .body(Empty::<Bytes>::new())
            .unwrap()
    };

    // Stream 1's headers are in while its body is still open upstream.
    let streaming = tokio::time::timeout(
        Duration::from_secs(5),
        sender.clone().send_request(get("/stream")),
    )
    .await
    .expect("stream 1 headers timed out")
    .unwrap();
    assert_eq!(streaming.status(), 200);

    let other = tokio::time::timeout(Duration::from_secs(5), async {
        let resp = sender.send_request(get("/other")).await.unwrap();
        resp.into_body().collect().await.unwrap().to_bytes()
    })
    .await
    .expect("stream 2 waited for stream 1's open response");
    assert_eq!(&other[..], b"other");

    release.notify_one();
    let body = tokio::time::timeout(Duration::from_secs(5), streaming.into_body().collect())
        .await
        .expect("stream 1 body timed out")
        .unwrap()
        .to_bytes();
    assert_eq!(&body[..], b"helloworld");

    proxy.task.abort();
    upstream.task.abort();
}

#[tokio::test]
async fn mitm_caps_upstream_connections_per_client_connection() {
    use std::time::Duration;

    use http_body_util::{BodyExt, Empty};
    use hyper::body::Bytes;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let ca = test_ca();
    let log_dir = TempDir::new().unwrap();
    let log_path = log_dir.path().join("proxy.log");
    let release = Arc::new(tokio::sync::Notify::new());
    let accepted = Arc::new(AtomicUsize::new(0));
    let upstream =
        start_held_stream_tls_upstream(ca, "localhost", release.clone(), accepted.clone()).await;
    let config_json = mitm_proxy_config_json(
        "denylist",
        &[rule("localhost", &["/never-block-this/**"], "block")],
        &log_path,
        ca,
    );
    let proxy = start_proxy(&config_json).await;

    let port = upstream.addr.port();
    let tls = connect_mitm_tls_alpn(proxy.local_addr, "localhost", port, ca, b"h2").await;
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    let get = |path: &str| {
        hyper::Request::get(format!("https://localhost{path}"))
            .body(Empty::<Bytes>::new())
            .unwrap()
    };

    // The first connection plus eight extra ones, each busy with an open
    // response that is kept until the end.
    let mut open = Vec::new();
    for _ in 0..9 {
        let resp =
            tokio::time::timeout(Duration::from_secs(5), sender.send_request(get("/stream")))
                .await
                .expect("response headers timed out")
                .unwrap();
        assert_eq!(resp.status(), 200);
        open.push(resp);
    }

    // The next two requests wait for one of them to come free, and then take
    // turns on it.
    let mut waiting = Box::pin(async {
        let first = sender.send_request(get("/other"));
        let second = sender.send_request(get("/other"));
        tokio::join!(first, second)
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut waiting)
            .await
            .is_err(),
        "requests did not wait for a busy connection to come free"
    );
    release.notify_one();
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("waiting requests were not sent once a connection came free");
    for resp in [first.unwrap(), second.unwrap()] {
        assert_eq!(resp.status(), 200);
        let body = tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
            .await
            .expect("body timed out")
            .unwrap()
            .to_bytes();
        assert_eq!(&body[..], b"other");
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 9);
    assert_eq!(open.len(), 9);

    proxy.task.abort();
    upstream.task.abort();
}
