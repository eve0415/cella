//! TLS MITM interception for HTTPS path-level blocking.
//!
//! When a domain has path-level blocking rules, the proxy intercepts the
//! TLS connection:
//! 1. Generate a per-domain certificate signed by the cella CA
//! 2. Accept TLS from the client using that certificate
//! 3. Parse decrypted HTTP requests to inspect URL paths
//! 4. Evaluate blocking rules against domain + path for every request
//! 5. If allowed, relay to upstream; if blocked, send 403

use std::sync::Arc;

use rcgen::{CertificateParams, DnType, DnValue, IsCa, Issuer, KeyPair, SanType};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::proxy_config::AgentProxyConfig;

/// Perform MITM interception on a CONNECT tunnel.
///
/// The client has already received "200 Connection Established" and expects
/// to start a TLS handshake. We accept TLS with a generated cert, then
/// inspect every HTTP/1.1 request on the connection for rule evaluation.
pub async fn intercept_tls(client: TcpStream, host: &str, port: u16, config: &AgentProxyConfig) {
    let tls_config = match generate_server_config(host, config) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("Failed to generate MITM cert for {host}: {e}");
            config.log_error(host, &format!("MITM cert generation failed: {e}"));
            return;
        }
    };

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let tls_stream = match acceptor.accept(client).await {
        Ok(s) => s,
        Err(e) => {
            warn!("TLS handshake failed for {host}: {e}");
            config.log_error(host, &format!("TLS handshake failed: {e}"));
            return;
        }
    };

    let (reader, mut writer) = tokio::io::split(tls_stream);
    let mut reader = BufReader::new(reader);

    // Read first request headers.
    let mut header_bytes = Vec::new();
    if read_request_headers(&mut reader, &mut header_bytes)
        .await
        .is_err()
    {
        return;
    }

    let (method, path) = parse_method_and_path(&header_bytes);
    let path = super::forward_proxy::strip_query(path);

    let verdict = config.matcher.evaluate(host, path);
    if !verdict.allowed {
        info!("BLOCKED HTTPS {host}{path} - {}", verdict.reason);
        config.log_blocked(host, path, &verdict.reason);
        let _ = writer
            .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }

    // Connect to upstream server.
    let upstream_tcp =
        match super::forward_proxy::connect_upstream_for_mitm(host, port, config).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to connect to {host}:{port} for MITM relay: {e}");
                let _ = writer.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            }
        };

    // Establish TLS to upstream.
    let connector = tokio_rustls::TlsConnector::from(Arc::new(upstream_tls_config()));
    let server_name = match rustls::pki_types::ServerName::try_from(host.to_string()) {
        Ok(sn) => sn,
        Err(e) => {
            warn!("Invalid server name {host}: {e}");
            return;
        }
    };

    let upstream_tls = match connector.connect(server_name, upstream_tcp).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Upstream TLS handshake failed for {host}: {e}");
            let _ = writer.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return;
        }
    };

    let (upstream_reader, mut upstream_writer) = tokio::io::split(upstream_tls);
    let mut upstream_reader = BufReader::new(upstream_reader);

    // Request loop: evaluate rules on every request, relay if allowed.
    let has_body = method_has_body(&method);
    let content_length = parse_content_length(&header_bytes);
    let is_chunked = is_chunked_transfer(&header_bytes);

    // Forward first request headers to upstream.
    if upstream_writer.write_all(&header_bytes).await.is_err() {
        return;
    }

    // Relay first request body.
    if has_body
        && let Err(e) = relay_body(
            &mut reader,
            &mut upstream_writer,
            content_length,
            is_chunked,
        )
        .await
    {
        debug!("Error relaying request body: {e}");
        return;
    }

    request_loop(
        host,
        config,
        &mut reader,
        &mut writer,
        &mut upstream_reader,
        &mut upstream_writer,
        &mut header_bytes,
    )
    .await;
}

/// Process subsequent HTTP/1.1 requests on a keep-alive MITM connection.
async fn request_loop<CR, CW, UR, UW>(
    host: &str,
    config: &AgentProxyConfig,
    reader: &mut CR,
    writer: &mut CW,
    upstream_reader: &mut UR,
    upstream_writer: &mut UW,
    header_bytes: &mut Vec<u8>,
) where
    CR: AsyncBufRead + Unpin,
    CW: AsyncWriteExt + Unpin,
    UR: AsyncBufRead + Unpin,
    UW: AsyncWriteExt + Unpin,
{
    loop {
        // Read response from upstream and relay to client.
        let mut resp_headers = Vec::new();
        if read_request_headers(upstream_reader, &mut resp_headers)
            .await
            .is_err()
        {
            break;
        }
        if writer.write_all(&resp_headers).await.is_err() {
            break;
        }

        let resp_content_length = parse_content_length(&resp_headers);
        let resp_chunked = is_chunked_transfer(&resp_headers);
        let keep_alive = !has_connection_close(&resp_headers);

        // Relay response body.
        if let Err(e) = relay_body(upstream_reader, writer, resp_content_length, resp_chunked).await
        {
            debug!("Error relaying response body: {e}");
            break;
        }

        if !keep_alive {
            break;
        }

        // Read next request from client.
        header_bytes.clear();
        if read_request_headers(reader, header_bytes).await.is_err() {
            break;
        }

        let (next_method, next_path) = parse_method_and_path(header_bytes);
        let next_path = super::forward_proxy::strip_query(next_path);

        // Evaluate rules on this request.
        let verdict = config.matcher.evaluate(host, next_path);
        if !verdict.allowed {
            info!(
                "BLOCKED HTTPS {host}{next_path} (keep-alive) - {}",
                verdict.reason
            );
            config.log_blocked(host, next_path, &verdict.reason);
            let _ = writer
                .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                .await;
            break;
        }

        // Forward allowed request to upstream.
        if upstream_writer.write_all(header_bytes).await.is_err() {
            break;
        }

        let has_body = method_has_body(&next_method);
        let content_length = parse_content_length(header_bytes);
        let is_chunked = is_chunked_transfer(header_bytes);

        if has_body
            && let Err(e) = relay_body(reader, upstream_writer, content_length, is_chunked).await
        {
            debug!("Error relaying request body: {e}");
            break;
        }
    }
}

/// Read HTTP headers (request or response) into `buf` until `\r\n\r\n`.
async fn read_request_headers<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<(), ()> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        let is_end = line == "\r\n" || line == "\n";
        buf.extend_from_slice(line.as_bytes());
        if is_end {
            return Ok(());
        }
    }
}

/// Parse method and path from request line bytes.
fn parse_method_and_path(header_bytes: &[u8]) -> (String, &str) {
    let header_str = std::str::from_utf8(header_bytes).unwrap_or("");
    let first_line = header_str.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or("GET").to_string();
    let path = if parts.len() >= 2 { parts[1] } else { "/" };
    (method, path)
}

/// Check if the HTTP method typically has a request body.
fn method_has_body(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH"
    )
}

/// Parse Content-Length from raw header bytes.
fn parse_content_length(headers: &[u8]) -> Option<u64> {
    let headers_str = std::str::from_utf8(headers).ok()?;
    for line in headers_str.lines() {
        if let Some(val) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            return val.trim().parse().ok();
        }
        // Case-insensitive check.
        if line.len() > 16 && line[..16].eq_ignore_ascii_case("content-length: ") {
            return line[16..].trim().parse().ok();
        }
    }
    None
}

/// Check if Transfer-Encoding: chunked is set.
fn is_chunked_transfer(headers: &[u8]) -> bool {
    let Ok(headers_str) = std::str::from_utf8(headers) else {
        return false;
    };
    for line in headers_str.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            return true;
        }
    }
    false
}

/// Check if Connection: close is set.
fn has_connection_close(headers: &[u8]) -> bool {
    let Ok(headers_str) = std::str::from_utf8(headers) else {
        return false;
    };
    for line in headers_str.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("connection:") && lower.contains("close") {
            return true;
        }
    }
    false
}

/// Relay an HTTP message body between reader and writer.
///
/// Handles Content-Length, chunked transfer encoding, and no-body cases.
async fn relay_body<R: AsyncBufRead + Unpin, W: AsyncWriteExt + Unpin>(
    reader: &mut R,
    writer: &mut W,
    content_length: Option<u64>,
    chunked: bool,
) -> Result<(), std::io::Error> {
    if chunked {
        relay_chunked(reader, writer).await
    } else if let Some(len) = content_length {
        relay_fixed(reader, writer, len).await
    } else {
        // No body (GET, HEAD, or 204/304 response).
        Ok(())
    }
}

/// Relay a fixed-length body.
async fn relay_fixed<R: AsyncRead + Unpin, W: AsyncWriteExt + Unpin>(
    reader: &mut R,
    writer: &mut W,
    length: u64,
) -> Result<(), std::io::Error> {
    let mut remaining = length;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let to_read = usize::try_from(remaining)
            .unwrap_or(buf.len())
            .min(buf.len());
        let n = reader.read(&mut buf[..to_read]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-body",
            ));
        }
        writer.write_all(&buf[..n]).await?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Relay a chunked transfer-encoded body.
///
/// Reads and forwards chunk headers + data + trailers verbatim.
async fn relay_chunked<R: AsyncBufRead + Unpin, W: AsyncWriteExt + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> Result<(), std::io::Error> {
    loop {
        // Read chunk size line.
        let mut size_line = String::new();
        reader.read_line(&mut size_line).await?;
        writer.write_all(size_line.as_bytes()).await?;

        let size_str = size_line.trim().split(';').next().unwrap_or("0");
        let chunk_size = u64::from_str_radix(size_str, 16).unwrap_or(0);

        if chunk_size == 0 {
            // Terminal chunk. Read trailing \r\n (end of chunked body).
            let mut trailer = String::new();
            reader.read_line(&mut trailer).await?;
            writer.write_all(trailer.as_bytes()).await?;
            break;
        }

        // Relay chunk data.
        relay_fixed(reader, writer, chunk_size).await?;

        // Read and relay trailing \r\n after chunk data.
        let mut crlf = String::new();
        reader.read_line(&mut crlf).await?;
        writer.write_all(crlf.as_bytes()).await?;
    }
    Ok(())
}

/// Generate a rustls `ServerConfig` with a certificate for the given domain,
/// signed by the cella CA.
fn generate_server_config(domain: &str, config: &AgentProxyConfig) -> Result<ServerConfig, String> {
    let ca = load_ca_materials(config)?;
    let ca_issuer = Issuer::from_params(&ca.params, &ca.key_pair);

    let domain_key = KeyPair::generate().map_err(|e| format!("key generation: {e}"))?;

    let mut params = CertificateParams::default();
    params.is_ca = IsCa::NoCa;
    params
        .distinguished_name
        .push(DnType::CommonName, DnValue::Utf8String(domain.to_string()));
    params.subject_alt_names.push(SanType::DnsName(
        domain
            .to_string()
            .try_into()
            .map_err(|e| format!("invalid domain for SAN: {e}"))?,
    ));

    let domain_cert = params
        .signed_by(&domain_key, &ca_issuer)
        .map_err(|e| format!("cert signing: {e}"))?;

    let cert_der = CertificateDer::from(domain_cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(domain_key.serialize_der()));

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| format!("server config: {e}"))
}

struct CaIssuerMaterials {
    params: CertificateParams,
    key_pair: KeyPair,
}

fn load_ca_materials(config: &AgentProxyConfig) -> Result<CaIssuerMaterials, String> {
    let ca_key_pem = config
        .ca_key_pem
        .as_deref()
        .ok_or("no CA key available for MITM")?;

    let ca_key_pair = KeyPair::from_pem(ca_key_pem).map_err(|e| format!("parse CA key: {e}"))?;

    Ok(CaIssuerMaterials {
        params: cella_network::ca::ca_certificate_params(),
        key_pair: ca_key_pair,
    })
}

fn upstream_tls_config() -> rustls::ClientConfig {
    let mut root_store = rustls::RootCertStore::empty();

    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = root_store.add(cert);
    }

    rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_length_header() {
        let headers = b"GET / HTTP/1.1\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(42));
    }

    #[test]
    fn parse_content_length_case_insensitive() {
        let headers = b"POST / HTTP/1.1\r\ncontent-length: 100\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(100));
    }

    #[test]
    fn parse_content_length_absent() {
        let headers = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert_eq!(parse_content_length(headers), None);
    }

    #[test]
    fn detect_chunked_transfer() {
        let headers = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(is_chunked_transfer(headers));
    }

    #[test]
    fn detect_connection_close() {
        let headers = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
        assert!(has_connection_close(headers));

        let headers = b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\n\r\n";
        assert!(!has_connection_close(headers));
    }

    #[test]
    fn parse_method_and_path_basic() {
        let headers = b"GET /api/v1 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/v1");
    }

    #[test]
    fn parse_method_and_path_with_query() {
        let headers = b"GET /api/v1?foo=bar HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/v1?foo=bar");
        // strip_query is applied by the caller
        assert_eq!(super::super::forward_proxy::strip_query(path), "/api/v1");
    }

    #[test]
    fn method_body_detection() {
        assert!(method_has_body("POST"));
        assert!(method_has_body("PUT"));
        assert!(method_has_body("PATCH"));
        assert!(!method_has_body("GET"));
        assert!(!method_has_body("HEAD"));
        assert!(!method_has_body("DELETE"));
    }

    // --- Additional header parsing edge cases ---

    #[test]
    fn parse_method_and_path_post() {
        let headers = b"POST /submit HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "POST");
        assert_eq!(path, "/submit");
    }

    #[test]
    fn parse_method_and_path_empty_bytes() {
        let headers = b"";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "GET");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_method_and_path_single_word() {
        let headers = b"CONNECT\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "CONNECT");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_content_length_mixed_case() {
        // Test the case-insensitive branch with "Content-length:" (not exact prefix match).
        let headers = b"GET / HTTP/1.1\r\nContent-length: 77\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(77));
    }

    #[test]
    fn parse_content_length_uppercase() {
        let headers = b"GET / HTTP/1.1\r\nCONTENT-LENGTH: 55\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(55));
    }

    #[test]
    fn parse_content_length_invalid_value() {
        let headers = b"GET / HTTP/1.1\r\nContent-Length: not-a-number\r\n\r\n";
        assert_eq!(parse_content_length(headers), None);
    }

    #[test]
    fn chunked_transfer_mixed_case() {
        let headers = b"POST / HTTP/1.1\r\nTRANSFER-ENCODING: chunked\r\n\r\n";
        assert!(is_chunked_transfer(headers));
    }

    #[test]
    fn chunked_transfer_not_set() {
        let headers = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert!(!is_chunked_transfer(headers));
    }

    #[test]
    fn chunked_transfer_invalid_utf8() {
        let headers = &[0xFF, 0xFE, 0xFD];
        assert!(!is_chunked_transfer(headers));
    }

    #[test]
    fn connection_close_mixed_case() {
        let headers = b"HTTP/1.1 200 OK\r\nCONNECTION: close\r\n\r\n";
        assert!(has_connection_close(headers));
    }

    #[test]
    fn connection_close_not_present() {
        let headers = b"HTTP/1.1 200 OK\r\n\r\n";
        assert!(!has_connection_close(headers));
    }

    #[test]
    fn connection_close_invalid_utf8() {
        let headers = &[0xFF, 0xFE];
        assert!(!has_connection_close(headers));
    }

    #[test]
    fn method_has_body_case_insensitive() {
        assert!(method_has_body("post"));
        assert!(method_has_body("Put"));
        assert!(method_has_body("PATCH"));
        assert!(!method_has_body("get"));
        assert!(!method_has_body("head"));
        assert!(!method_has_body("OPTIONS"));
        assert!(!method_has_body("TRACE"));
    }

    // --- Async relay tests ---

    #[tokio::test]
    async fn read_request_headers_basic() {
        let data = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let result = read_request_headers(&mut reader, &mut buf).await;
        assert!(result.is_ok());
        assert_eq!(buf, data);
    }

    #[tokio::test]
    async fn read_request_headers_empty_stream() {
        let data = b"";
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let result = read_request_headers(&mut reader, &mut buf).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_request_headers_multiple_headers() {
        let data = b"POST /api HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: 5\r\n\r\n";
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let result = read_request_headers(&mut reader, &mut buf).await;
        assert!(result.is_ok());
        assert_eq!(buf, data);
    }

    #[tokio::test]
    async fn relay_fixed_body() {
        let body = b"hello world";
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        let result = relay_fixed(&mut reader, &mut writer, body.len() as u64).await;
        assert!(result.is_ok());
        assert_eq!(writer, body);
    }

    #[tokio::test]
    async fn relay_fixed_body_empty() {
        let body = b"";
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        let result = relay_fixed(&mut reader, &mut writer, 0).await;
        assert!(result.is_ok());
        assert!(writer.is_empty());
    }

    #[tokio::test]
    async fn relay_fixed_body_premature_eof() {
        let body = b"short";
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        let result = relay_fixed(&mut reader, &mut writer, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn relay_body_no_body() {
        let data = b"";
        let mut reader = BufReader::new(&data[..]);
        let mut writer = Vec::new();
        // No content-length, not chunked: should be a no-op.
        let result = relay_body(&mut reader, &mut writer, None, false).await;
        assert!(result.is_ok());
        assert!(writer.is_empty());
    }

    #[tokio::test]
    async fn relay_body_with_content_length() {
        let body = b"abcde";
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        let result = relay_body(&mut reader, &mut writer, Some(5), false).await;
        assert!(result.is_ok());
        assert_eq!(writer, b"abcde");
    }

    #[tokio::test]
    async fn relay_chunked_body() {
        // A valid chunked body: one chunk of "hello" (5 bytes) then terminal chunk.
        let chunked = b"5\r\nhello\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&chunked[..]);
        let mut writer = Vec::new();
        let result = relay_chunked(&mut reader, &mut writer).await;
        assert!(result.is_ok());
        assert_eq!(writer, chunked);
    }

    #[tokio::test]
    async fn relay_body_chunked_mode() {
        let chunked = b"3\r\nabc\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&chunked[..]);
        let mut writer = Vec::new();
        let result = relay_body(&mut reader, &mut writer, None, true).await;
        assert!(result.is_ok());
        assert_eq!(writer, chunked);
    }

    // --- Additional edge cases for parse_method_and_path ---

    #[test]
    fn parse_method_and_path_delete() {
        let headers = b"DELETE /api/resource/42 HTTP/1.1\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "DELETE");
        assert_eq!(path, "/api/resource/42");
    }

    #[test]
    fn parse_method_and_path_options() {
        let headers = b"OPTIONS * HTTP/1.1\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "OPTIONS");
        assert_eq!(path, "*");
    }

    #[test]
    fn parse_method_and_path_connect() {
        let headers = b"CONNECT example.com:443 HTTP/1.1\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "CONNECT");
        assert_eq!(path, "example.com:443");
    }

    #[test]
    fn parse_method_and_path_put_with_long_path() {
        let headers = b"PUT /api/v2/users/12345/settings/preferences HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "PUT");
        assert_eq!(path, "/api/v2/users/12345/settings/preferences");
    }

    #[test]
    fn parse_method_and_path_patch() {
        let headers = b"PATCH /resource HTTP/1.1\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "PATCH");
        assert_eq!(path, "/resource");
    }

    #[test]
    fn parse_method_and_path_head() {
        let headers = b"HEAD / HTTP/1.1\r\n\r\n";
        let (method, path) = parse_method_and_path(headers);
        assert_eq!(method, "HEAD");
        assert_eq!(path, "/");
    }

    // --- Content-Length edge cases ---

    #[test]
    fn parse_content_length_with_extra_whitespace() {
        let headers = b"POST / HTTP/1.1\r\nContent-Length:   99  \r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(99));
    }

    #[test]
    fn parse_content_length_zero() {
        let headers = b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(0));
    }

    #[test]
    fn parse_content_length_large_value() {
        let headers = b"POST / HTTP/1.1\r\nContent-Length: 4294967296\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(4_294_967_296));
    }

    #[test]
    fn parse_content_length_invalid_utf8() {
        let headers = &[0xFF, 0xFE, 0xFD];
        assert_eq!(parse_content_length(headers), None);
    }

    #[test]
    fn parse_content_length_among_many_headers() {
        let headers = b"POST /api HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\nContent-Length: 256\r\nContent-Type: application/json\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(256));
    }

    // --- Transfer-Encoding edge cases ---

    #[test]
    fn chunked_transfer_with_extra_encoding() {
        let headers = b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        assert!(is_chunked_transfer(headers));
    }

    #[test]
    fn chunked_transfer_not_chunked_encoding() {
        let headers = b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n";
        assert!(!is_chunked_transfer(headers));
    }

    // --- Connection: close edge cases ---

    #[test]
    fn connection_keep_alive_is_not_close() {
        let headers = b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\n\r\n";
        assert!(!has_connection_close(headers));
    }

    #[test]
    fn no_connection_header_is_not_close() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
        assert!(!has_connection_close(headers));
    }

    // --- Async header reading edge cases ---

    #[tokio::test]
    async fn read_request_headers_lf_only() {
        // Some servers use bare LF.
        let data = b"GET / HTTP/1.1\nHost: example.com\n\n";
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let result = read_request_headers(&mut reader, &mut buf).await;
        assert!(result.is_ok());
        assert_eq!(buf, data);
    }

    #[tokio::test]
    async fn read_request_headers_stops_at_separator() {
        // Data after the header separator should not be consumed.
        let data = b"GET / HTTP/1.1\r\n\r\nbody data here";
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let result = read_request_headers(&mut reader, &mut buf).await;
        assert!(result.is_ok());
        assert_eq!(buf, b"GET / HTTP/1.1\r\n\r\n");
    }

    // --- Relay body edge cases ---

    #[tokio::test]
    async fn relay_body_content_length_takes_precedence_over_no_body() {
        let body = b"hello";
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        // Content-Length is set but chunked is false.
        let result = relay_body(&mut reader, &mut writer, Some(5), false).await;
        assert!(result.is_ok());
        assert_eq!(writer, b"hello");
    }

    #[tokio::test]
    async fn relay_chunked_multiple_chunks() {
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&chunked[..]);
        let mut writer = Vec::new();
        let result = relay_chunked(&mut reader, &mut writer).await;
        assert!(result.is_ok());
        assert_eq!(writer, chunked);
    }

    #[tokio::test]
    async fn relay_chunked_with_extension() {
        // Chunk with extension (;ext=val) should still work.
        let chunked = b"3;ext=val\r\nabc\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&chunked[..]);
        let mut writer = Vec::new();
        let result = relay_chunked(&mut reader, &mut writer).await;
        assert!(result.is_ok());
        assert_eq!(writer, chunked);
    }

    #[tokio::test]
    async fn relay_fixed_exact_size() {
        let body = b"exact";
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        let result = relay_fixed(&mut reader, &mut writer, 5).await;
        assert!(result.is_ok());
        assert_eq!(writer, b"exact");
    }

    #[tokio::test]
    async fn relay_fixed_large_body() {
        // Body larger than internal buffer size (8192).
        let body = vec![0x42u8; 16384];
        let mut reader = BufReader::new(&body[..]);
        let mut writer = Vec::new();
        let result = relay_fixed(&mut reader, &mut writer, 16384).await;
        assert!(result.is_ok());
        assert_eq!(writer.len(), 16384);
        assert!(writer.iter().all(|&b| b == 0x42));
    }
}
