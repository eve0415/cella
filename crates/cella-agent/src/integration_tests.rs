//! Integration tests for proxy-mediated network policy enforcement.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

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
}

async fn start_upstream_server() -> UpstreamServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind upstream server");
    let addr = listener.local_addr().expect("upstream local addr");

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut headers = Vec::new();
                let mut byte = [0_u8; 1];
                while stream.read_exact(&mut byte).await.is_ok() {
                    headers.push(byte[0]);
                    if headers.ends_with(b"\r\n\r\n") || headers.ends_with(b"\n\n") {
                        break;
                    }
                }

                let body = b"upstream ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body).await;
            });
        }
    });

    UpstreamServer { addr, task }
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
