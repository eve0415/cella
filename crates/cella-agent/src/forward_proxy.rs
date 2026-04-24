//! HTTP forward proxy for domain and path blocking.
//!
//! Implements an HTTP CONNECT proxy that runs inside the container.
//! For HTTPS, uses CONNECT tunneling. For HTTP, proxies directly.
//! Evaluates blocking rules against each request.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::proxy_config::AgentProxyConfig;

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

/// Handle a single proxy connection.
///
/// Wraps the stream in a `BufReader` to avoid consuming bytes beyond the HTTP
/// headers (e.g., the TLS `ClientHello` for CONNECT requests).
async fn handle_connection(stream: TcpStream, config: Arc<AgentProxyConfig>) {
    let mut reader = BufReader::new(stream);

    // Read headers line by line until \r\n\r\n.
    let mut header_bytes = Vec::new();
    let mut first_line = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) => {
                debug!("Proxy read error: {e}");
                return;
            }
        }
        if first_line.is_empty() {
            first_line = line.clone();
        }
        header_bytes.extend_from_slice(line.as_bytes());
        if line == "\r\n" || line == "\n" {
            break;
        }
    }

    let trimmed = first_line.trim_end();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        let _ = reader
            .get_mut()
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await;
        return;
    }

    let method = parts[0].to_string();
    let target = parts[1].to_string();

    if method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(reader, &target, &config).await;
    } else {
        handle_direct_http(reader, &header_bytes, &method, &target, &config).await;
    }
}

/// Handle CONNECT tunneling (for HTTPS).
///
/// CONNECT requests look like: `CONNECT host:port HTTP/1.1`
/// We evaluate domain blocking rules before establishing the tunnel.
/// For domains with path-level rules, we defer to MITM TLS interception.
async fn handle_connect(mut client: BufReader<TcpStream>, target: &str, config: &AgentProxyConfig) {
    // Parse host:port from CONNECT target.
    let Some((host, port)) = parse_host_port(target) else {
        let _ = client
            .get_mut()
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await;
        return;
    };

    let needs_path_inspection = config.matcher.domain_needs_path_inspection(&host);
    let has_mitm = config.ca_cert_pem.is_some() && config.ca_key_pem.is_some();

    if needs_path_inspection && has_mitm {
        // Domain has path-level rules and MITM is available — allow the CONNECT
        // and defer the actual allow/block decision to the MITM path where we
        // can inspect request paths.
        let mut client_tcp = client.into_inner();
        if client_tcp
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        crate::mitm::intercept_tls(client_tcp, &host, port, config).await;
        return;
    }

    if needs_path_inspection && config.warn_no_mitm_once(&host) {
        warn!(
            "CONNECT to {host}: path-level rules exist but MITM is unavailable; \
             path blocking disabled (requires TLS interception)"
        );
    }

    // Evaluate domain-level rules. The "/" path won't trigger path-specific
    // rules, so domain blocks still apply even without MITM.
    let verdict = config.matcher.evaluate(&host, "/");
    if !verdict.allowed {
        info!("BLOCKED CONNECT to {host}:{port} - {}", verdict.reason);
        config.log_blocked(&host, "/", &verdict.reason);
        let _ = client
            .get_mut()
            .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            .await;
        return;
    }

    // Tunnel without MITM.
    let upstream = match connect_upstream(&host, port, config).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to {host}:{port}: {e}");
            let _ = client
                .get_mut()
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await;
            return;
        }
    };

    // Headers have been fully consumed by BufReader; safe to unwrap to raw TcpStream.
    let mut client_tcp = client.into_inner();
    if client_tcp
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }

    // Tunnel bidirectionally.
    let mut upstream = upstream;
    let _ = copy_bidirectional(&mut client_tcp, &mut upstream).await;
}

/// Handle direct HTTP proxy request (non-CONNECT).
///
/// Direct requests look like: `GET http://host/path HTTP/1.1`
/// We evaluate both domain and path blocking rules.
async fn handle_direct_http(
    mut client: BufReader<TcpStream>,
    header_bytes: &[u8],
    _method: &str,
    target: &str,
    config: &AgentProxyConfig,
) {
    // Parse the URL to extract host and path.
    let Some((host, port, path)) = parse_http_url(target) else {
        let _ = client
            .get_mut()
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await;
        return;
    };

    // Evaluate rules with both domain and path (strip query/fragment).
    let verdict = config.matcher.evaluate(&host, strip_query(&path));
    if !verdict.allowed {
        info!("BLOCKED HTTP {target} - {}", verdict.reason);
        config.log_blocked(&host, &path, &verdict.reason);
        let _ = client
            .get_mut()
            .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            .await;
        return;
    }

    // Connect to upstream and forward the request.
    let mut upstream = match connect_upstream(&host, port, config).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to {host}:{port}: {e}");
            let _ = client
                .get_mut()
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await;
            return;
        }
    };

    // Forward the original request headers to upstream.
    if upstream.write_all(header_bytes).await.is_err() {
        return;
    }

    // Bidirectional copy for the rest (request body + response).
    let mut client_tcp = client.into_inner();
    let _ = copy_bidirectional(&mut client_tcp, &mut upstream).await;
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
    let connect_req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
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

/// Strip query string and fragment from a URL path before rule evaluation.
pub fn strip_query(path: &str) -> &str {
    let end = path
        .find('?')
        .unwrap_or(path.len())
        .min(path.find('#').unwrap_or(path.len()));
    &path[..end]
}

/// Parse `host:port` from a CONNECT target.
fn parse_host_port(target: &str) -> Option<(String, u16)> {
    if let Some((host, port_str)) = target.rsplit_once(':') {
        let port: u16 = port_str.parse().ok()?;
        Some((host.to_string(), port))
    } else {
        Some((target.to_string(), 443))
    }
}

/// Parse an HTTP URL to extract host, port, and path.
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (host_port, path) = rest
        .find('/')
        .map_or((rest, "/"), |idx| (&rest[..idx], &rest[idx..]));

    let (host, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
        (h.to_string(), p.parse().ok()?)
    } else {
        (host_port.to_string(), 80)
    };

    Some((host, port, path.to_string()))
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
    fn parse_connect_target() {
        let (host, port) = parse_host_port("example.com:443").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_connect_target_no_port() {
        let (host, port) = parse_host_port("example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_http_url_with_path() {
        let (host, port, path) = parse_http_url("http://example.com:8080/api/v1").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 8080);
        assert_eq!(path, "/api/v1");
    }

    #[test]
    fn parse_http_url_default_port() {
        let (host, port, path) = parse_http_url("http://example.com/index.html").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/index.html");
    }

    #[test]
    fn parse_http_url_no_path() {
        let (host, port, path) = parse_http_url("http://example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/");
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
    fn strip_query_removes_query_string() {
        assert_eq!(strip_query("/api/v1?foo=bar"), "/api/v1");
        assert_eq!(strip_query("/api/v1#section"), "/api/v1");
        assert_eq!(strip_query("/api/v1?foo=bar#section"), "/api/v1");
        assert_eq!(strip_query("/api/v1#section?foo=bar"), "/api/v1");
        assert_eq!(strip_query("/api/v1"), "/api/v1");
        assert_eq!(strip_query("/"), "/");
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
    fn parse_host_port_non_standard() {
        let (host, port) = parse_host_port("myhost:8443").unwrap();
        assert_eq!(host, "myhost");
        assert_eq!(port, 8443);
    }

    #[test]
    fn parse_host_port_invalid_port() {
        assert!(parse_host_port("myhost:notaport").is_none());
    }

    #[test]
    fn parse_host_port_port_overflow() {
        // u16 max is 65535
        assert!(parse_host_port("myhost:99999").is_none());
    }

    #[test]
    fn parse_http_url_https_prefix_rejected() {
        assert!(parse_http_url("https://example.com/path").is_none());
    }

    #[test]
    fn parse_http_url_no_scheme() {
        assert!(parse_http_url("example.com/path").is_none());
    }

    #[test]
    fn parse_http_url_with_query_string() {
        let (host, port, path) = parse_http_url("http://example.com/api?key=val").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/api?key=val");
    }

    #[test]
    fn parse_http_url_invalid_port() {
        assert!(parse_http_url("http://example.com:abc/path").is_none());
    }

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

    #[test]
    fn strip_query_empty_path() {
        assert_eq!(strip_query(""), "");
    }

    #[test]
    fn strip_query_only_question_mark() {
        assert_eq!(strip_query("?"), "");
    }

    #[test]
    fn strip_query_only_hash() {
        assert_eq!(strip_query("#"), "");
    }

    #[test]
    fn strip_query_path_with_both() {
        assert_eq!(strip_query("/page?q=1#top"), "/page");
    }

    #[test]
    fn strip_query_hash_before_question() {
        assert_eq!(strip_query("/page#section?q=1"), "/page");
    }
}
