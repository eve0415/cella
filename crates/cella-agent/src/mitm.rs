//! TLS MITM interception for HTTPS path-level blocking.
//!
//! When a domain has path-level blocking rules, the proxy intercepts the
//! TLS connection:
//! 1. Generate a per-domain certificate signed by the cella CA
//! 2. Accept TLS from the client using that certificate
//! 3. Parse the decrypted HTTP request to inspect the URL path
//! 4. Evaluate blocking rules against domain + path
//! 5. If allowed, establish TLS to the upstream and relay traffic

use std::sync::Arc;

use rcgen::{CertificateParams, DnType, DnValue, IsCa, Issuer, KeyPair, SanType};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::proxy_config::AgentProxyConfig;

/// Perform MITM interception on a CONNECT tunnel.
///
/// The client has already received "200 Connection Established" and expects
/// to start a TLS handshake. We accept TLS with a generated cert, read the
/// HTTP request inside, evaluate rules, and either block or forward.
pub async fn intercept_tls(client: TcpStream, host: &str, port: u16, config: &AgentProxyConfig) {
    // Generate a certificate for this domain.
    let tls_config = match generate_server_config(host, config) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("Failed to generate MITM cert for {host}: {e}");
            return;
        }
    };

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    // Accept TLS from the client.
    let tls_stream = match acceptor.accept(client).await {
        Ok(s) => s,
        Err(e) => {
            debug!("TLS handshake failed for {host}: {e}");
            return;
        }
    };

    let (reader, mut writer) = tokio::io::split(tls_stream);
    let mut reader = BufReader::new(reader);

    // Read the HTTP request line (e.g., "GET /path HTTP/1.1\r\n").
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() || request_line.is_empty() {
        return;
    }

    // Read remaining headers.
    let mut headers = Vec::new();
    headers.push(request_line.clone());
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let is_end = line == "\r\n" || line == "\n";
                headers.push(line);
                if is_end {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    // Parse method and path from request line.
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let path = if parts.len() >= 2 { parts[1] } else { "/" };

    // Evaluate rules with domain + path.
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

    let mut upstream_tls = match connector.connect(server_name, upstream_tcp).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Upstream TLS handshake failed for {host}: {e}");
            let _ = writer.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return;
        }
    };

    // Forward the original request to upstream.
    let header_bytes: Vec<u8> = headers.iter().flat_map(|h| h.as_bytes().to_vec()).collect();
    if upstream_tls.write_all(&header_bytes).await.is_err() {
        return;
    }

    // Bidirectional relay between client TLS and upstream TLS.
    let mut client_tls = reader.into_inner().unsplit(writer);
    let _ = tokio::io::copy_bidirectional(&mut client_tls, &mut upstream_tls).await;
}

/// Generate a rustls `ServerConfig` with a certificate for the given domain,
/// signed by the cella CA.
fn generate_server_config(domain: &str, config: &AgentProxyConfig) -> Result<ServerConfig, String> {
    let ca = load_ca_materials(config)?;
    let ca_issuer = Issuer::from_params(&ca.params, &ca.key_pair);

    // Generate a key pair for this domain.
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

/// CA issuer materials for signing domain certs.
struct CaIssuerMaterials {
    params: CertificateParams,
    key_pair: KeyPair,
}

/// Load the CA cert params and key from PEM strings.
fn load_ca_materials(config: &AgentProxyConfig) -> Result<CaIssuerMaterials, String> {
    let ca_key_pem = config
        .ca_key_pem
        .as_deref()
        .ok_or("no CA key available for MITM")?;

    let ca_key_pair = KeyPair::from_pem(ca_key_pem).map_err(|e| format!("parse CA key: {e}"))?;

    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Cella Dev Container CA");

    Ok(CaIssuerMaterials {
        params: ca_params,
        key_pair: ca_key_pair,
    })
}

/// Build a TLS client config for connecting to upstream servers.
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
