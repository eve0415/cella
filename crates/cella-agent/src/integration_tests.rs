//! Integration tests for proxy-mediated network policy enforcement.

use std::net::SocketAddr;
use std::path::Path;
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
        let _ = rustls::crypto::ring::default_provider().install_default();

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
                        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    if tls.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                    if tls.write_all(&body).await.is_err() {
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
