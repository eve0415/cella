//! HTTP forward proxy for domain and path blocking.
//!
//! Serves each client connection with hyper. CONNECT requests are tunnelled
//! or handed to TLS interception; absolute-form `http://` requests are
//! evaluated one by one, so every request on a keep-alive connection is
//! checked against the rules before it is forwarded.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{HOST, HeaderValue};
use hyper::http::uri::{PathAndQuery, Scheme};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use cella_network::authority::{HostPort, parse_connect_target, parse_plain_authority};

use crate::proxy_config::AgentProxyConfig;
use crate::proxy_http::{
    BoxBody, GatewayError, Pooled, closing_response, has_invalid_token_lines, is_websocket_upgrade,
    offer_only_websocket, relay_upgrade, send_or_reconnect, strip_hop_by_hop, text_response,
};
use crate::respond::{self, Channel, Decision};

/// Most upstream connections one client connection keeps open at once.
const MAX_POOLED_UPSTREAMS: usize = 8;

/// Upstream HTTP/1.1 connections of one client connection, keyed by
/// (lowercased host, port) and capped at [`MAX_POOLED_UPSTREAMS`], least
/// recently used first.
#[derive(Default)]
struct UpstreamPool {
    entries: Vec<(UpstreamKey, SendRequest<Incoming>)>,
}

type UpstreamKey = (String, u16);

impl UpstreamPool {
    fn take(&mut self, key: &UpstreamKey) -> Option<SendRequest<Incoming>> {
        let index = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(index).1)
    }

    /// Store `sender` as the most recently used entry, dropping the least
    /// recently used one (which closes its connection) past the cap.
    fn put(&mut self, key: UpstreamKey, sender: SendRequest<Incoming>) {
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push((key, sender));
        if self.entries.len() > MAX_POOLED_UPSTREAMS {
            self.entries.remove(0);
        }
    }
}

/// Running forward proxy listener.
pub struct ForwardProxyHandle {
    /// Address the proxy actually bound to.
    pub local_addr: SocketAddr,
    /// Background accept-loop task.
    #[allow(dead_code)]
    pub task: JoinHandle<()>,
}

/// Start the forward proxy server.
///
/// Listens on `127.0.0.1:<port>` and handles HTTP CONNECT and direct HTTP
/// proxy requests. Returns the task handle.
///
/// # Errors
///
/// Returns an error if binding fails.
pub async fn start_forward_proxy(
    config: Arc<AgentProxyConfig>,
) -> Result<ForwardProxyHandle, io::Error> {
    let port = config.listen_port;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let local_addr = listener.local_addr()?;
    info!("Forward proxy listening on {local_addr}");

    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    debug!("Proxy connection from {peer}");
                    let cfg = config.clone();
                    tokio::spawn(handle_connection(stream, cfg));
                }
                Err(e) => {
                    warn!("Proxy accept error: {e}");
                }
            }
        }
    });

    Ok(ForwardProxyHandle { local_addr, task })
}

/// Serve one client connection, evaluating every request it carries.
async fn handle_connection(stream: TcpStream, config: Arc<AgentProxyConfig>) {
    let pool: Arc<Mutex<UpstreamPool>> = Arc::default();
    let svc = service_fn(move |req: Request<Incoming>| {
        let config = config.clone();
        let pool = pool.clone();
        async move {
            let resp = if req.method() == Method::CONNECT {
                handle_connect(req, &config).await
            } else {
                handle_plain(req, &config, &pool).await
            };
            Ok::<_, Infallible>(resp)
        }
    });

    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.half_close(true);
    if let Err(e) = builder
        .serve_connection(TokioIo::new(stream), svc)
        .with_upgrades()
        .await
    {
        debug!("Proxy connection ended: {e}");
    }
}

/// Handle CONNECT tunneling (for HTTPS).
///
/// Domain rules are evaluated before the tunnel is established. Domains whose
/// rules need decrypted traffic (path patterns, `respond`, response headers)
/// or credential routing go to MITM TLS interception.
/// Refusals close the connection, since hyper would otherwise keep it open
/// after a non-2xx CONNECT reply.
async fn handle_connect(
    req: Request<Incoming>,
    config: &Arc<AgentProxyConfig>,
) -> Response<BoxBody> {
    let Some((host, port)) = req
        .uri()
        .authority()
        .and_then(|a| parse_connect_target(a.as_str()).ok())
        .map(|target| (target.host, target.port))
    else {
        return closing_response(StatusCode::BAD_REQUEST, "Bad Request");
    };

    let is_credential_domain = config.credential_route_for_domain(&host).is_some();
    let needs_mitm = config.matcher.domain_needs_mitm(&host);
    let has_mitm = config.ca_cert_pem.is_some() && config.ca_key_pem.is_some();

    if is_credential_domain && !has_mitm {
        warn!(
            "CONNECT to {host}: credential protection requires TLS interception but MITM is unavailable"
        );
        return closing_response(
            StatusCode::BAD_GATEWAY,
            "Credential protection requires TLS interception (no MITM).",
        );
    }

    if (is_credential_domain || needs_mitm) && has_mitm {
        let config = config.clone();
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    crate::mitm::intercept_tls(TokioIo::new(upgraded), &host, port, config).await;
                }
                Err(e) => debug!("CONNECT upgrade for {host} failed: {e}"),
            }
        });
        return Response::new(crate::proxy_http::full(""));
    }

    // When MITM is unavailable, evaluate domain-level rules only — path
    // rules are skipped entirely rather than matched against a fake path,
    // and a `respond` rule for the domain blocks the tunnel.
    let verdict = if needs_mitm {
        if config.warn_no_mitm_once(&host) {
            warn!(
                "CONNECT to {host}: path, respond or response-header rules need TLS \
                 interception but MITM is unavailable; path rules and response headers \
                 are not applied, and respond rules block the domain"
            );
        }
        config.matcher.evaluate_domain_only(&host)
    } else {
        config.matcher.evaluate(&host, "/")
    };
    if !verdict.is_allowed() {
        info!("BLOCKED CONNECT to {host}:{port} - {}", verdict.reason);
        config.log_blocked(&host, "/", &verdict.reason);
        return closing_response(StatusCode::FORBIDDEN, "Forbidden");
    }

    config.log_allowed(&host, "/");

    // Tunnel without MITM.
    let mut upstream = match connect_upstream(&host, port, config).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to {host}:{port}: {e}");
            return closing_response(StatusCode::BAD_GATEWAY, "Bad Gateway");
        }
    };
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut client = TokioIo::new(upgraded);
                if let Err(e) = copy_bidirectional(&mut client, &mut upstream).await {
                    debug!("Tunnel to {host}:{port} ended: {e}");
                }
            }
            Err(e) => debug!("CONNECT upgrade for {host} failed: {e}"),
        }
    });
    Response::new(crate::proxy_http::full(""))
}

/// Target of an absolute-form `http://` request.
///
/// Any userinfo in the request URI is dropped: only the host and port are
/// kept, for `Host`, the upstream pool key, and log text.
struct PlainTarget {
    /// Lowercased host; an IPv6 literal in canonical form without brackets.
    host: String,
    port: u16,
    /// Host as written, lowercased, with an IPv6 literal in brackets, plus
    /// `:port` unless it is the default 80: the `Host` value.
    host_header: String,
}

impl PlainTarget {
    /// Only absolute-form `http://` targets are accepted. The `Host` header is
    /// never used as a fallback: it would let a client point the proxy at
    /// itself.
    fn from_uri(uri: &Uri) -> Option<Self> {
        if uri.scheme() != Some(&Scheme::HTTP) {
            return None;
        }
        let HostPort {
            host,
            written,
            port,
        } = parse_plain_authority(uri.authority()?.as_str()).ok()?;
        let written = written.to_ascii_lowercase();
        let host_header = if port == cella_network::authority::HTTP_DEFAULT_PORT {
            written
        } else {
            format!("{written}:{port}")
        };
        Some(Self {
            host,
            port,
            host_header,
        })
    }

    fn key(&self) -> UpstreamKey {
        (self.host.clone(), self.port)
    }
}

/// Handle one plain-HTTP proxy request (non-CONNECT).
async fn handle_plain(
    req: Request<Incoming>,
    config: &AgentProxyConfig,
    pool: &Mutex<UpstreamPool>,
) -> Response<BoxBody> {
    // A field line `header_tokens` would skip could hide an `Upgrade` token
    // or a header listed for stripping.
    if has_invalid_token_lines(req.headers()) {
        return text_response(StatusCode::BAD_REQUEST, "Bad Request");
    }
    let Some(target) = PlainTarget::from_uri(req.uri()) else {
        return text_response(StatusCode::BAD_REQUEST, "Bad Request");
    };

    let (req, response_headers) =
        match respond::decide(req, &target.host, config, Channel::PlainHttp).await {
            Decision::Done(resp) => return resp,
            Decision::Forward(req, response_headers) => (req, response_headers),
        };

    // Only WebSocket upgrades are spliced. Any other upgrade (such as h2c) is
    // forwarded as an ordinary request with its upgrade headers stripped, so
    // later requests on the connection still go through the rules.
    let forwarded = if is_websocket_upgrade(&req) {
        forward_plain_upgrade(req, &target, config).await
    } else {
        forward_plain(req, &target, config, pool).await
    };
    respond::forwarded_response(forwarded, response_headers, &target.host, config)
}

/// Rewrite a client request for the upstream.
///
/// Without an upstream proxy the request goes out in origin-form; through an
/// upstream proxy it goes out in absolute-form without userinfo. `Host`
/// always names the target host and port (RFC 9112 §3.2.2), which also
/// covers HTTP/1.0 clients that omit it.
fn prepare_plain_request(
    req: Request<Incoming>,
    target: &PlainTarget,
    config: &AgentProxyConfig,
    preserve_upgrade: bool,
) -> Request<Incoming> {
    let (mut parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"));
    parts.uri = if config.upstream_proxy.is_none() {
        Uri::from(pq)
    } else {
        Uri::builder()
            .scheme(Scheme::HTTP)
            .authority(target.host_header.as_str())
            .path_and_query(pq)
            .build()
            .unwrap_or_else(|_| parts.uri.clone())
    };
    strip_hop_by_hop(&mut parts.headers, preserve_upgrade);
    if let Ok(host) = HeaderValue::from_str(&target.host_header) {
        parts.headers.insert(HOST, host);
    }
    Request::from_parts(parts, body)
}

/// Forward an allowed plain-HTTP request over the connection's pool.
async fn forward_plain(
    req: Request<Incoming>,
    target: &PlainTarget,
    config: &AgentProxyConfig,
    pool: &Mutex<UpstreamPool>,
) -> Result<Response<BoxBody>, GatewayError> {
    let req = prepare_plain_request(req, target, config, false);
    let key = target.key();
    let mut pooled = take_ready_sender(pool, &key).await;
    let sending = match pooled.as_mut() {
        Some(sender) => Pooled::Sent(sender.try_send_request(req)),
        None => Pooled::Unsent(Box::new(req)),
    };

    let mut fresh = None;
    let fresh_slot = &mut fresh;
    let reconnect = move |req| async move {
        let mut sender = connect_plain_upstream(target, config).await?;
        let sent = sender.send_request(req);
        *fresh_slot = Some(sender);
        sent.await.map_err(|e| e.to_string())
    };
    match send_or_reconnect(sending, reconnect, &target.host_header).await {
        Ok(mut resp) => {
            if let Some(sender) = fresh.or(pooled) {
                pool.lock().await.put(key, sender);
            }
            strip_hop_by_hop(resp.headers_mut(), false);
            Ok(resp.map(|body| body.map_err(Into::into).boxed()))
        }
        Err(e) => {
            warn!("Upstream request to {} failed: {e}", target.host_header);
            Err(GatewayError::BadGateway)
        }
    }
}

/// Take the pooled sender for `key` if its upstream connection is still usable.
async fn take_ready_sender(
    pool: &Mutex<UpstreamPool>,
    key: &UpstreamKey,
) -> Option<SendRequest<Incoming>> {
    let mut sender = pool.lock().await.take(key)?;
    sender.ready().await.ok()?;
    Some(sender)
}

async fn connect_plain_upstream(
    target: &PlainTarget,
    config: &AgentProxyConfig,
) -> Result<SendRequest<Incoming>, String> {
    let tcp = connect_upstream(&target.host, target.port, config)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let host = target.host_header.clone();
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!("Upstream connection to {host} ended: {e}");
        }
    });
    Ok(sender)
}

/// Forward a plain-HTTP WebSocket upgrade (`ws://`) on a dedicated upstream
/// connection.
async fn forward_plain_upgrade(
    mut req: Request<Incoming>,
    target: &PlainTarget,
    config: &AgentProxyConfig,
) -> Result<Response<BoxBody>, GatewayError> {
    let client_upgrade = hyper::upgrade::on(&mut req);
    let mut req = prepare_plain_request(req, target, config, true);
    offer_only_websocket(req.headers_mut());
    let resp = match connect_plain_upstream(target, config).await {
        Ok(mut sender) => sender.send_request(req).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    match resp {
        Ok(resp) => relay_upgrade(client_upgrade, resp, &target.host_header),
        Err(e) => {
            warn!("Upgrade request to {} failed: {e}", target.host_header);
            Err(GatewayError::BadGateway)
        }
    }
}
/// Connect to the target, either directly or through an upstream proxy.
/// Public for use by the MITM module.
pub async fn connect_upstream_for_mitm(
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
) -> Result<TcpStream, io::Error> {
    connect_upstream(host, port, config).await
}

/// Connect to the target, either directly or through an upstream proxy.
async fn connect_upstream(
    host: &str,
    port: u16,
    config: &AgentProxyConfig,
) -> Result<TcpStream, io::Error> {
    if let Some(ref upstream_proxy) = config.upstream_proxy {
        connect_via_upstream_proxy(host, port, upstream_proxy).await
    } else {
        TcpStream::connect((host.to_string().as_str(), port)).await
    }
}

/// Connect through an upstream HTTP proxy using CONNECT.
async fn connect_via_upstream_proxy(
    host: &str,
    port: u16,
    proxy_url: &str,
) -> Result<TcpStream, io::Error> {
    // Parse proxy URL to get host:port.
    let proxy_addr = parse_proxy_url(proxy_url)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid upstream proxy URL"))?;

    let mut proxy_stream = TcpStream::connect(proxy_addr.as_str()).await?;

    // Send CONNECT request to upstream proxy.
    let target = host_port(host, port);
    let connect_req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    proxy_stream.write_all(connect_req.as_bytes()).await?;

    // Read the response (expect "HTTP/1.1 200").
    let mut resp_buf = [0u8; 4096];
    let n = proxy_stream.read(&mut resp_buf).await?;
    let resp = String::from_utf8_lossy(&resp_buf[..n]);

    if !resp.starts_with("HTTP/1.1 200") && !resp.starts_with("HTTP/1.0 200") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "upstream proxy rejected CONNECT: {}",
                resp.lines().next().unwrap_or("")
            ),
        ));
    }

    Ok(proxy_stream)
}

/// `host:port` as written in an authority: an IPv6 literal goes in brackets.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Parse a proxy URL like `http://host:port` to `host:port`.
fn parse_proxy_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    // Strip trailing slash and path.
    let host_port = rest.split('/').next()?;
    // Strip auth (user:pass@host:port).
    let host_port = host_port
        .rfind('@')
        .map_or(host_port, |idx| &host_port[idx + 1..]);
    Some(host_port.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_target_unbrackets_ipv6_host() {
        let uri: Uri = "http://[::1]:8080/path".parse().unwrap();
        let target = PlainTarget::from_uri(&uri).unwrap();
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 8080);
        assert_eq!(target.host_header, "[::1]:8080");

        let uri: Uri = "http://[::1]/".parse().unwrap();
        let target = PlainTarget::from_uri(&uri).unwrap();
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 80);
        assert_eq!(target.host_header, "[::1]");
    }

    #[test]
    fn plain_target_rejects_invalid_port() {
        for uri in [
            "http://example.com:65536/path",
            "http://example.com:abc/",
            "http://example.com:+80/",
            "http://[::1]:abc/",
        ] {
            let uri: Uri = uri.parse().unwrap();
            assert!(PlainTarget::from_uri(&uri).is_none(), "{uri} was accepted");
        }
        let uri: Uri = "http://user:pw@example.com/".parse().unwrap();
        let target = PlainTarget::from_uri(&uri).unwrap();
        assert_eq!((target.host.as_str(), target.port), ("example.com", 80));
    }

    #[test]
    fn empty_port_means_scheme_default() {
        for (uri, host_header) in [
            ("http://Example.com:/path", "example.com"),
            ("http://[::1]:/", "[::1]"),
        ] {
            let uri: Uri = uri.parse().unwrap();
            let target = PlainTarget::from_uri(&uri).unwrap();
            assert_eq!(target.port, 80, "{uri}");
            assert_eq!(target.host_header, host_header, "{uri}");
        }
    }

    #[test]
    fn unspecified_address_is_rejected() {
        for target in ["[::]", "[0::0]", "[::ffff:0.0.0.0]", "0.0.0.0"] {
            let uri: Uri = format!("http://{target}:80/").parse().unwrap();
            assert!(PlainTarget::from_uri(&uri).is_none(), "{target}");
        }
    }

    #[test]
    fn empty_host_is_rejected() {
        let uri: Uri = "http://:8080/x".parse().unwrap();
        assert!(PlainTarget::from_uri(&uri).is_none());
    }

    #[test]
    fn host_port_brackets_ipv6_literal() {
        assert_eq!(host_port("::1", 443), "[::1]:443");
        assert_eq!(host_port("example.com", 443), "example.com:443");
    }

    #[test]
    fn parse_proxy_url_simple() {
        assert_eq!(
            parse_proxy_url("http://proxy:3128"),
            Some("proxy:3128".to_string())
        );
    }

    #[test]
    fn parse_proxy_url_with_auth() {
        assert_eq!(
            parse_proxy_url("http://user:pass@proxy:3128"),
            Some("proxy:3128".to_string())
        );
    }

    #[test]
    fn parse_proxy_url_with_path() {
        assert_eq!(
            parse_proxy_url("http://proxy:3128/"),
            Some("proxy:3128".to_string())
        );
    }

    // --- Additional edge case tests ---

    #[test]
    fn parse_proxy_url_no_scheme() {
        assert!(parse_proxy_url("proxy:3128").is_none());
    }

    #[test]
    fn parse_proxy_url_https_scheme() {
        assert_eq!(
            parse_proxy_url("https://secure-proxy:443"),
            Some("secure-proxy:443".to_string())
        );
    }

    #[test]
    fn parse_proxy_url_with_trailing_path() {
        assert_eq!(
            parse_proxy_url("http://proxy:3128/some/path"),
            Some("proxy:3128".to_string())
        );
    }
}
